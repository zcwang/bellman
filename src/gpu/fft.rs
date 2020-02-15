use crate::gpu::{
    error::{GPUError, GPUResult},
    locks, sources, structs, utils, GPU_NVIDIA_DEVICES,
};
use crossbeam::thread;
use ff::Field;
use log::info;
use ocl::{Buffer, Device, MemFlags, ProQue};
use paired::Engine;
use std::cmp;

// NOTE: Please read `structs.rs` for an explanation for unsafe transmutes of this code!

const LOG2_MAX_ELEMENTS: usize = 32; // At most 2^32 elements is supported.
const MAX_RADIX_DEGREE: u32 = 8; // Radix256
const MAX_LOCAL_WORK_SIZE_DEGREE: u32 = 7; // 128

pub struct SingleFFTKernel<E>
where
    E: Engine,
{
    proque: ProQue,
    fft_src_buffer: Buffer<structs::PrimeFieldStruct<E::Fr>>,
    fft_dst_buffer: Buffer<structs::PrimeFieldStruct<E::Fr>>,
    fft_pq_buffer: Buffer<structs::PrimeFieldStruct<E::Fr>>,
    fft_omg_buffer: Buffer<structs::PrimeFieldStruct<E::Fr>>,
    core_count: usize,
    _lock: locks::GPULock, // RFC 1857: struct fields are dropped in the same order as they are declared.
}

impl<E> SingleFFTKernel<E>
where
    E: Engine,
{
    pub fn create(d: Device, n: u32) -> GPUResult<SingleFFTKernel<E>> {
        let lock = locks::GPULock::lock();

        let src = sources::kernel::<E>();
        let pq = ProQue::builder().device(d).src(src).dims(n).build()?;
        let core_count = utils::get_core_count(d)?;

        let srcbuff = Buffer::builder()
            .queue(pq.queue().clone())
            .flags(MemFlags::new().read_write())
            .len(n)
            .build()?;
        let dstbuff = Buffer::builder()
            .queue(pq.queue().clone())
            .flags(MemFlags::new().read_write())
            .len(n)
            .build()?;
        let pqbuff = Buffer::builder()
            .queue(pq.queue().clone())
            .flags(MemFlags::new().read_write())
            .len(1 << MAX_RADIX_DEGREE >> 1)
            .build()?;
        let omgbuff = Buffer::builder()
            .queue(pq.queue().clone())
            .flags(MemFlags::new().read_write())
            .len(LOG2_MAX_ELEMENTS)
            .build()?;

        Ok(SingleFFTKernel {
            proque: pq,
            fft_src_buffer: srcbuff,
            fft_dst_buffer: dstbuff,
            fft_pq_buffer: pqbuff,
            fft_omg_buffer: omgbuff,
            core_count: core_count,
            _lock: lock,
        })
    }

    /// Peforms a FFT round
    /// * `lgn` - Specifies log2 of number of elements
    /// * `lgp` - Specifies log2 of `p`, (http://www.bealto.com/gpu-fft_group-1.html)
    /// * `deg` - 1=>radix2, 2=>radix4, 3=>radix8, ...
    /// * `max_deg` - The precalculated values pq` and `omegas` are valid for radix degrees up to `max_deg`
    fn radix_fft_round(
        &mut self,
        lgn: u32,
        lgp: u32,
        deg: u32,
        max_deg: u32,
        in_src: bool,
    ) -> ocl::Result<()> {
        let n = 1u32 << lgn;
        let lwsd = cmp::min(deg - 1, MAX_LOCAL_WORK_SIZE_DEGREE);
        let kernel = self
            .proque
            .kernel_builder("radix_fft")
            .global_work_size([n >> deg << lwsd])
            .local_work_size(1 << lwsd)
            .arg(if in_src {
                &self.fft_src_buffer
            } else {
                &self.fft_dst_buffer
            })
            .arg(if in_src {
                &self.fft_dst_buffer
            } else {
                &self.fft_src_buffer
            })
            .arg(&self.fft_pq_buffer)
            .arg(&self.fft_omg_buffer)
            .arg_local::<structs::PrimeFieldStruct<E::Fr>>(1 << deg)
            .arg(n)
            .arg(lgp)
            .arg(deg)
            .arg(max_deg)
            .build()?;
        unsafe {
            kernel.enq()?;
        } // Running a GPU kernel is unsafe!
        Ok(())
    }

    /// Share some precalculated values between threads to boost the performance
    fn setup_pq(&mut self, omega: &E::Fr, n: usize, max_deg: u32) -> ocl::Result<()> {
        // Precalculate:
        // [omega^(0/(2^(deg-1))), omega^(1/(2^(deg-1))), ..., omega^((2^(deg-1)-1)/(2^(deg-1)))]
        let mut tpq = vec![structs::PrimeFieldStruct::<E::Fr>::default(); 1 << max_deg >> 1];
        let pq = unsafe {
            std::mem::transmute::<&mut [structs::PrimeFieldStruct<E::Fr>], &mut [E::Fr]>(&mut tpq)
        };
        let tw = omega.pow([(n >> max_deg) as u64]);
        pq[0] = E::Fr::one();
        if max_deg > 1 {
            pq[1] = tw;
            for i in 2..(1 << max_deg >> 1) {
                pq[i] = pq[i - 1];
                pq[i].mul_assign(&tw);
            }
        }
        self.fft_pq_buffer.write(&tpq).enq()?;

        // Precalculate [omega, omega^2, omega^4, omega^8, ..., omega^(2^31)]
        let mut tom = vec![structs::PrimeFieldStruct::<E::Fr>::default(); 32];
        let om = unsafe {
            std::mem::transmute::<&mut [structs::PrimeFieldStruct<E::Fr>], &mut [E::Fr]>(&mut tom)
        };
        om[0] = *omega;
        for i in 1..LOG2_MAX_ELEMENTS {
            om[i] = om[i - 1].pow([2u64]);
        }
        self.fft_omg_buffer.write(&tom).enq()?;

        Ok(())
    }

    /// Performs FFT on `a`
    /// * `omega` - Special value `omega` is used for FFT over finite-fields
    /// * `lgn` - Specifies log2 of number of elements
    pub fn radix_fft(&mut self, a: &mut [E::Fr], omega: &E::Fr, lgn: u32) -> GPUResult<()> {
        let n = 1 << lgn;

        let ta = unsafe {
            std::mem::transmute::<&mut [E::Fr], &mut [structs::PrimeFieldStruct<E::Fr>]>(a)
        };

        let max_deg = cmp::min(MAX_RADIX_DEGREE, lgn);
        self.setup_pq(omega, n, max_deg)?;

        self.fft_src_buffer.write(&*ta).enq()?;
        let mut in_src = true;
        let mut lgp = 0u32;
        while lgp < lgn {
            let deg = cmp::min(max_deg, lgn - lgp);
            self.radix_fft_round(lgn, lgp, deg, max_deg, in_src)?;
            lgp += deg;
            in_src = !in_src; // Destination of this FFT round is source of the next round.
        }
        if in_src {
            self.fft_src_buffer.read(ta).enq()?;
        } else {
            self.fft_dst_buffer.read(ta).enq()?;
        }
        self.proque.finish()?; // Wait for all commands in the queue (Including read command)

        Ok(())
    }
}

// A struct that containts several multiexp kernels for different devices
pub struct FFTKernel<E>
where
    E: Engine,
{
    kernels: Vec<SingleFFTKernel<E>>,
}

impl<E> FFTKernel<E>
where
    E: Engine,
{
    pub fn create(n: u32) -> GPUResult<FFTKernel<E>> {
        let kernels: Vec<_> = GPU_NVIDIA_DEVICES
            .iter()
            .map(|d| SingleFFTKernel::<E>::create(*d, n))
            .filter(|res| res.is_ok())
            .map(|res| res.unwrap())
            .collect();
        if kernels.is_empty() {
            return Err(GPUError::Simple("No working GPUs found!"));
        }
        info!("FFT: {} working device(s) selected.", kernels.len());
        for (i, k) in kernels.iter().enumerate() {
            info!("FFT: Device {}: {}", i, k.proque.device().name()?,);
        }
        return Ok(FFTKernel::<E> { kernels });
    }

    pub fn radix_fft(&mut self, sets: &mut Vec<(&mut [E::Fr], &E::Fr, u32)>) -> GPUResult<()> {
        let num_ffts = sets.len();
        let num_devices = self.kernels.len();
        let chunk_size = ((num_ffts as f64) / (num_devices as f64)).ceil() as usize;

        match thread::scope(|s| -> Result<(), GPUError> {
            let mut threads = Vec::new();

            if num_ffts > 0 {
                for (chunk, kern) in sets.chunks_mut(chunk_size).zip(self.kernels.iter_mut()) {
                    threads.push(s.spawn(move |_| -> Result<(), GPUError> {
                        for (a, omega, lgn) in chunk.iter_mut() {
                            kern.radix_fft(a, omega, *lgn)?;
                        }
                        Ok(())
                    }));
                }
            }

            let mut results = vec![];
            for t in threads {
                results.push(t.join());
            }
            for r in results {
                r??;
            }

            Ok(())
        }) {
            Ok(_) => Ok(()),
            Err(e) => Err(GPUError::from(e)),
        }
    }
}
