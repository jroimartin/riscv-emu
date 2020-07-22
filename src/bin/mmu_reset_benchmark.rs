use riscv_emu::mmu;
use std::time::Instant;

const NUM_ITER: usize = 1000;

fn bench_mmu_fork() -> f64 {
    let mmu = mmu::Mmu::new(4 * 1024 * 1024);

    let start = Instant::now();

    for _ in 0..NUM_ITER {
        mmu.fork();
    }

    NUM_ITER as f64 / start.elapsed().as_secs_f64()
}

fn bench_mmu_reset() -> f64 {
    let mmu_init = mmu::Mmu::new(4 * 1024 * 1024);
    let mut mmu_fork = mmu_init.fork();

    let start = Instant::now();

    for _ in 0..NUM_ITER {
        mmu_fork.reset(&mmu_init);
    }

    NUM_ITER as f64 / start.elapsed().as_secs_f64()
}

fn main() {
    println!("bench_mmu_fork:  {:12.2} ops", bench_mmu_fork());
    println!("bench_mmu_reset: {:12.2} ops", bench_mmu_reset());
}
