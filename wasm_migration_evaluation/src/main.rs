use std::fs::File;
use std::io::{Read, Write};
use wasmtime::{Caller, Config, Engine, Extern, Linker, Module, Store};
use wasmtime_wasi::{preview1, WasiCtxBuilder};
use wasmtime_wasi::preview1::WasiP1Ctx;
use wasm_migrate::{WasmMigrate, ComputationInfo};
use crate::config::{BenchmarkInfo, Benchmarks};
use affinity;
use std::env;

mod config;

// PolyBenchC experiment
fn inject_checkpoint_and_restore_procedures(benchmark: &BenchmarkInfo, distributed: bool) // -> ComputationInfo
{
    println!("Injecting benchmark checkpoint for {}...", benchmark.name);

    let mut migration_injector: WasmMigrate = WasmMigrate::new();
    migration_injector.component_index = 0;
    migration_injector.core_module_index = 0;
    migration_injector.func_index = benchmark.func_index as i32;

    // Load the module Wasm bytecode.
    let path_to_file =
        format!("./wasm_migration_evaluation/wasm_bunch/{}.wasm", benchmark.name);
    let path_to_dest =
        format!("./wasm_migration_evaluation/migrating_bunch/{}.wasm", benchmark.name);
    let mut file = File::open(path_to_file).unwrap();
    let mut wasm_bytes = Vec::new();
    let _ = file.read_to_end(&mut wasm_bytes);

    // Inject the procedures into the input wasm bytes.
    let binding = wasm_bytes.clone();
    let modified_body =
        migration_injector.compute(&binding, distributed).clone();

    // Then save the resulting module.
    let mut output_file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path_to_dest).expect("Unable to save the file. ");

    let _ = output_file.write(modified_body.as_slice());

    // Return the information acquired for the computation.
    // migration_injector.print_module_info(benchmark.func_index, benchmark.name.clone())
}

fn run_benchmark(benchmark: &BenchmarkInfo, path_to_dir: String) {
    struct MyState {
        wasi: WasiP1Ctx,
    }

    // Create the engine.
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let engine = Engine::default();

    // Load the module.
    let path_to_module = format!("{}/{}.wasm", path_to_dir, benchmark.name);
    let module =
        Module::from_file(&engine, path_to_module).expect("Failed to load wasm file. ");

    // Create the Linker, with a simple callback to signal a migration request.
    let mut linker: Linker<MyState>  = Linker::new(&engine);
    preview1::add_to_linker_sync(&mut linker, |cx| &mut cx.wasi)
        .expect("add_to_linker_sync failed. ");

    // Add the should_migrate function, that for this experiment always return 0.
    linker.func_wrap("host", "should_migrate", || { 0 } )
        .expect("func_wrap failed. ");

    // Add the restore_memory, which in this experiment do nothing.
    linker.func_wrap("host", "restore_memory", || {  } )
        .expect("add_to_linker_sync failed. ");

    let pre = linker.instantiate_pre(&module).expect("instantiate failed. ");

    // Create the Store.
    let wasi_ctx = WasiCtxBuilder::new()
        .inherit_stdio()
        .inherit_env()
        .args(&args)
        .build_p1();
    let state = MyState {
        wasi: wasi_ctx,
    };
    let mut store = Store::new(&engine, state);

    // Instantiate the module.
    let instance = pre.instantiate(&mut store).expect("instantiate failed. ");

    // Invoke the start function of the module.
    let func = instance.get_func(&mut store, "_start")
        .expect("Unable to find function _start. ");

    let mut result = [];
    let _r = func.call(&mut store, &[], &mut result);
    // println!("Return value: {:?}", _r);
    assert_eq!(_r.unwrap(), ());
}

#[allow(unsafe_code)]
fn run_checkpoint_and_restore_benchmark(benchmark: &BenchmarkInfo, path_to_dir: String) {
    struct MyState {
        wasi: WasiP1Ctx,
        checkpoint_num: i32,
        is_resuming: i32,
    }

    // Create the engine.
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let engine = Engine::default();

    // Load the module.
    let path_to_module = format!("{}/{}.wasm", path_to_dir, benchmark.name);
    let module =
        Module::from_file(&engine, path_to_module).expect("Failed to load wasm file. ");

    // Create the Linker, with a simple callback to signal a migration request.
    let mut linker: Linker<MyState>  = Linker::new(&engine);
    preview1::add_to_linker_sync(&mut linker, |cx| &mut cx.wasi)
        .expect("add_to_linker_sync failed. ");

    // Add the should_migrate function, that will be triggered after two checkpoints.
    let max_checkoint_num = 2;
    linker.func_wrap("host", "should_migrate", move |mut caller: Caller<'_, MyState>| {
        if caller.data_mut().checkpoint_num > max_checkoint_num {
            caller.data_mut().checkpoint_num = 0;
            1
        }
        else {
            caller.data_mut().checkpoint_num += 1;
            0
        }
    } ).expect("func_wrap failed. ");

    // Add the restore_memory, which do nothing right now.
    linker.func_wrap("host", "restore_memory", || { } )
        .expect("func_wrap failed. ");

    let pre = linker.instantiate_pre(&module).expect("instantiate failed. ");

    // Create the Store.
    let wasi_ctx = WasiCtxBuilder::new()
        .inherit_stdio()
        .inherit_env()
        .args(&args)
        .build_p1();
    let state = MyState {
        wasi: wasi_ctx,
        checkpoint_num: 0,
        is_resuming: 0,
    };
    let mut store = Store::new(&engine, state);

    // Instantiate the module.
    let instance = pre.instantiate(&mut store).expect("instantiate failed. ");

    // Invoke the start function of the module.
    let func = instance.get_func(&mut store, "_start")
        .expect("Unable to find function _start. ");
    let mut result = [];
    let _r = func.call(&mut store, &[], &mut result);
    println!("Checkpoint! ");

    // Save the memory.
    let checkpoint_memory = instance.get_memory(&mut store, "checkpoint_memory").expect("Unable to find memory");
    let checkpoint_data = checkpoint_memory.data_mut(&mut store).to_vec();

    let main_linear_memory = instance.get_memory(&mut store, "memory").expect("Unable to find memory");
    let main_lin_mem_data = main_linear_memory.data(&mut store).to_vec();

    // Redo all the instantiation step, as if from a different host.
    // Create the engine.
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let engine = Engine::default();

    // Load the module.
    let path_to_module = format!("{}/{}.wasm", path_to_dir, benchmark.name);
    let module =
        Module::from_file(&engine, path_to_module).expect("Failed to load wasm file. ");

    // Create the Linker, with a simple callback to signal a migration request.
    let mut linker: Linker<MyState>  = Linker::new(&engine);
    preview1::add_to_linker_sync(&mut linker, |cx| &mut cx.wasi)
        .expect("add_to_linker_sync failed. ");

    // Add the should_migrate function, this time, it will never signal a pending migration.
    linker.func_wrap("host", "should_migrate", || { 0 } )
        .expect("func_wrap failed. ");

    // Add the restore_memory, this time, it will be invoked.
    let main_lin_mem_export = module.get_export_index("memory").expect("Unable to find export");
    let checkpoint_mem_export = module.get_export_index("checkpoint_memory").expect("Unable to find export");

    linker.func_wrap("host", "restore_memory", move |mut caller: Caller<'_, MyState>| unsafe {
        if caller.data().is_resuming == 1 {
            // Get the host address of the main linear memory (and repeat for additional ones).
            let main_linear_memory = match caller.get_module_export(&main_lin_mem_export) {
                Some(Extern::Memory(mem)) => mem,
                _ => panic!("failed to find host memory"),
            };
            let main_lin_mem = main_linear_memory.data_ptr(&caller);

            // Get the host address of the checkpoint memory.
            let checkpoint_mem = match caller.get_module_export(&checkpoint_mem_export) {
                Some(Extern::Memory(mem)) => mem,
                _ => panic!("failed to find host memory"),
            };
            let checkpoint_memory = checkpoint_mem.data_ptr(&caller);

            // Copy the main linear memory from its checkpoint.
            for i in 0..main_lin_mem_data.len() {
                *main_lin_mem.wrapping_add(i) = main_lin_mem_data[i];
            }

            // Copy the checkpoint memory.
            for i in 0..checkpoint_data.len() {
                *checkpoint_memory.wrapping_add(i) = checkpoint_data[i];
            }
        }
    } ).expect("func_wrap failed. ");

    let pre = linker.instantiate_pre(&module).expect("instantiate failed. ");

    // Create the Store.
    let wasi_ctx = WasiCtxBuilder::new()
        .inherit_stdio()
        .inherit_env()
        .args(&args)
        .build_p1();
    let state = MyState {
        wasi: wasi_ctx,
        checkpoint_num: 0,
        is_resuming: 0,
    };
    let mut store = Store::new(&engine, state);

    // Instantiate the module.
    let instance = pre.instantiate(&mut store).expect("instantiate failed. ");

    // Invoke the start function of the module.
    let func = instance.get_func(&mut store, "_start")
        .expect("Unable to find function _start. ");
    let mut result = [];
    let _r = func.call(&mut store, &[], &mut result);

    let func = instance.get_func(&mut store, "_start")
        .expect("Unable to find function _start. ");
    let mut result = [];
    let _r = func.call(&mut store, &[], &mut result);
}

fn main() -> () {
    let args: Vec<String> = env::args().collect();
    let number_of_samples_str = &args[1];
    let distributed_as_string = &args[2];
    let run_original_as_string = &args[3];
    let number_of_samples = number_of_samples_str.parse::<i32>().unwrap();
    let distributed = distributed_as_string.parse::<bool>().unwrap();
    let run_original = run_original_as_string.parse::<bool>().unwrap();
    println!(" - The experiment start - ");
    println!("Configuration: samples         = {} ", number_of_samples);
    println!("               distributed C/R = {}", distributed);
    println!("               original        = {}", run_original);

    let cores: Vec<usize> = vec![8];
    affinity::set_thread_affinity(&cores).unwrap();

    /*  PolyBenchC experiment */

    // Register the computation configs.
    let benchmarks = Benchmarks::new();

    // Vector with the data acquired during the checkpoint injection.
    // let mut benchmarks_data = vec![];

    // Checkpoint and restore injection.
    for benchmark in &benchmarks.benchmarks {
        // let comp_info = 
        inject_checkpoint_and_restore_procedures(benchmark, distributed);
        // benchmarks_data.push(comp_info);
    }

    // Save the data acquired so far.
    // let path_to_report = "./wasm_migration_evaluation/report/report.json";
    // let mut report = File::create(path_to_report).expect("Unable to save the file. ");
    // let json_data = serde_json::to_string_pretty(&benchmarks_data).unwrap();
    // let _result = report.write_all(json_data.as_bytes());

    // Run each computation and measure the execution times.
    // let number_of_sample = 20;
    // #[derive(serde::Serialize)]
    struct ExecutionTime {
        computation: String,
        execution_time: Vec<f64>,
    }
    let mut original_comp_times : Vec<ExecutionTime> = vec![];
    let mut migrating_comp_times : Vec<ExecutionTime> = vec![];

    let path_to_wasm_dir = "./wasm_migration_evaluation/wasm_bunch";
    let path_to_mig_dir = "./wasm_migration_evaluation/migrating_bunch";

    fn run_all_benchmarks(
        benchmarks: &Benchmarks,
        dest: String,
        samples: i32,
        results: &mut Vec<ExecutionTime>)
    {
        for benchmark in &benchmarks.benchmarks {
            println!("Running benchmark {}", benchmark.name);
            /* let mut exec_time = ExecutionTime {
                computation: benchmark.name.clone(),
                execution_time: vec![],
            }; */
            for _s in 0..samples {
                run_benchmark(&benchmark, dest.to_string());
                // exec_time.execution_time.push(time);
            }
            // results.push(exec_time);
        }
    }

    if run_original {
        // Acquire the execution time for the original computation.
        println!("Acquiring the execution time for the original computation... ");
        run_all_benchmarks(
            &benchmarks,
            path_to_wasm_dir.to_string(),
            number_of_samples,
            &mut original_comp_times);
    }
    else {
        // Acquire the execution time for the migrating computation.
        println!("Acquiring the execution time for the migrating computation... ");
        run_all_benchmarks(
            &benchmarks,
            path_to_mig_dir.to_string(),
            number_of_samples,
            &mut migrating_comp_times);
    }
    
    println!("Hello, world!");
    
}