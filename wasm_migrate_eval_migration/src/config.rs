
/// A single benchmark.
#[derive(Debug, Clone)]
pub struct BenchmarkInfo {
    /// The name of the computation.
    pub name: String,

    /// The index of the kernel function.
    pub func_index: u32,
}

/// The list of benchmarks to evaluate.
pub struct Benchmarks {
    pub benchmarks: Vec<BenchmarkInfo>,
}

impl Benchmarks {
    pub fn new() -> Self {
        Benchmarks {
            benchmarks: [
                BenchmarkInfo { name: "2mm".to_string(),            func_index: 12 },
                BenchmarkInfo { name: "3mm".to_string(),            func_index: 12 },
                BenchmarkInfo { name: "adi".to_string(),            func_index: 12 },
                BenchmarkInfo { name: "atax".to_string(),           func_index: 12 },
                BenchmarkInfo { name: "bicg".to_string(),           func_index: 12 },
                BenchmarkInfo { name: "cholesky".to_string(),       func_index: 12 },
                BenchmarkInfo { name: "correlation".to_string(),    func_index: 12 },
                BenchmarkInfo { name: "covariance".to_string(),     func_index: 12 },
                BenchmarkInfo { name: "deriche".to_string(),        func_index: 12 },
                BenchmarkInfo { name: "doitgen".to_string(),        func_index: 10 },
                BenchmarkInfo { name: "durbin".to_string(),         func_index: 12 },
                BenchmarkInfo { name: "fdtd-2d".to_string(),        func_index: 12 },
                BenchmarkInfo { name: "floyd-warshall".to_string(), func_index: 12 },
                BenchmarkInfo { name: "gemm".to_string(),           func_index: 12 },
                BenchmarkInfo { name: "gemver".to_string(),         func_index: 12 },
                BenchmarkInfo { name: "gesummv".to_string(),        func_index: 12 },
                BenchmarkInfo { name: "gramschmidt".to_string(),    func_index: 12 },
                BenchmarkInfo { name: "heat-3d".to_string(),        func_index: 12 },
                BenchmarkInfo { name: "jacobi-1d".to_string(),      func_index: 12 },
                BenchmarkInfo { name: "jacobi-2d".to_string(),      func_index: 12 },
                BenchmarkInfo { name: "lu".to_string(),             func_index: 12 },
                BenchmarkInfo { name: "ludcmp".to_string(),         func_index: 12 },
                BenchmarkInfo { name: "mvt".to_string(),            func_index: 12 },
                BenchmarkInfo { name: "nussinov".to_string(),       func_index: 12 },
                BenchmarkInfo { name: "seidel-2d".to_string(),      func_index: 12 },
                BenchmarkInfo { name: "symm".to_string(),           func_index: 12 },
                BenchmarkInfo { name: "syrk".to_string(),           func_index: 12 },
                BenchmarkInfo { name: "trisolv".to_string(),        func_index: 12 },
                BenchmarkInfo { name: "trmm".to_string(),           func_index: 12 },
            ].to_vec(),
        }
    }
}