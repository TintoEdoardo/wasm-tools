mod config;

use std::env;
use std::fs::File;
use wasmtime::{Caller, Engine, Extern, Linker, Module, Store};
use wasmtime_wasi::{preview1, WasiCtxBuilder};
use wasmtime_wasi::preview1::WasiP1Ctx;
use crate::config::{BenchmarkInfo, Benchmarks};
use affinity;
use rustix;
use wasm_migrate::WasmMigrate;
use std::io::{Read, Write};

// Insert the checkpoint procedures.
fn insert_checkpoint_and_restore(benchmark: &BenchmarkInfo, distributed: bool)

{
    let mut migration_injector: WasmMigrate = WasmMigrate::new();
    migration_injector.component_index = 0;
    migration_injector.core_module_index = 0;
    migration_injector.func_index = benchmark.func_index as i32;

    // Load the module Wasm bytecode.
    let path_to_file =
        format!("./wasm_migrate_eval_migration/wasm_bunch/{}.wasm", benchmark.name);
    let path_to_dest =
        format!("./wasm_migrate_eval_migration/migrating_bunch/{}.wasm", benchmark.name);
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
}

fn timespec_diff(t1: rustix::time::Timespec, t2: rustix::time::Timespec) -> f64 {
    ( (t2.tv_nsec.abs_diff(t1.tv_nsec)) + t2.tv_sec.abs_diff(t1.tv_sec) * 1_000_000_000) as f64
}

#[allow(unsafe_code)]
fn run_and_migrate(benchmark: &BenchmarkInfo, path_to_dir: String, regions_before_checkpoint: i32) {

    // Prepare the times we plan to acquire, all in nanoseconds.
    let pre_instantiation_time : f64;
    let instantiation_time : f64;
    let execution_time_before_migration : f64;
    let get_checkpoint_memory_time : f64;
    let checkpoint_memory_copy_time : f64;
    let get_main_memory_time : f64;
    let main_memory_copy_time : f64;
    let pre_instantiation_time_after : f64;
    let instantiation_time_after : f64;
    let execution_time_after_migration : f64;

    let mut t1: rustix::time::Timespec;
    let mut t2: rustix::time::Timespec;

    // First, fix this computation to a specific CPU.
    // In this case, CPU 8.
    let cores: Vec<usize> = vec![8];
    affinity::set_thread_affinity(&cores).unwrap();

    // Then start executing the computation.
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
    linker.func_wrap("host", "should_migrate", move |mut caller: Caller<'_, MyState>| {
        if caller.data_mut().checkpoint_num > regions_before_checkpoint {
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

    // Compute the pre_instance time.
    t1 = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let pre = linker.instantiate_pre(&module).expect("instantiate failed. ");
    t2 = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    pre_instantiation_time = timespec_diff(t1, t2);

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
    t1 = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let instance = pre.instantiate(&mut store).expect("instantiate failed. ");
    t2 = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    instantiation_time = timespec_diff(t1, t2);

    // Invoke the start function of the module.
    let func = instance.get_func(&mut store, "_start")
        .expect("Unable to find function _start. ");
    let mut result = [];

    t1 = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let _r = func.call(&mut store, &[], &mut result);
    t2 = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    execution_time_before_migration = timespec_diff(t1, t2);
    println!("Checkpoint! ");

    // Save the memory.
    t1 = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let checkpoint_memory = instance.get_memory(&mut store, "checkpoint_memory").expect("Unable to find memory");
    t2 = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    get_checkpoint_memory_time = timespec_diff(t1, t2);

    t1 = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let checkpoint_data = checkpoint_memory.data_mut(&mut store).to_vec();
    t2 = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    checkpoint_memory_copy_time = timespec_diff(t1, t2);

    t1 = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let main_linear_memory = instance.get_memory(&mut store, "memory").expect("Unable to find memory");
    t2 = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    get_main_memory_time = timespec_diff(t1, t2);

    t1 = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let main_lin_mem_data = main_linear_memory.data(&mut store).to_vec();
    t2 = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    main_memory_copy_time = timespec_diff(t1, t2);

    // ------- Migration happens here! ------- //

    // However, instead of changing host, we are changing core.
    // This time, we move to a core that has different L2 cache
    // associated.
    let cores: Vec<usize> = vec![4];
    affinity::set_thread_affinity(&cores).unwrap();

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
            let t1 = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
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
            let t2 = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
            print!("Time to copy memory = {}. ", timespec_diff(t1, t2));
        }
    } ).expect("func_wrap failed. ");

    // Get the pre_instantiate time.
    t1 = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let pre = linker.instantiate_pre(&module).expect("instantiate failed. ");
    t2 = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    pre_instantiation_time_after = timespec_diff(t1, t2);

    // Create the Store.
    let wasi_ctx = WasiCtxBuilder::new()
        .inherit_stdio()
        .inherit_env()
        .args(&args)
        .build_p1();
    let state = MyState {
        wasi: wasi_ctx,
        checkpoint_num: 0,
        is_resuming: 1,
    };
    let mut store = Store::new(&engine, state);

    // Instantiate the module.
    t1 = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let instance = pre.instantiate(&mut store).expect("instantiate failed. ");
    t2 = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    instantiation_time_after = timespec_diff(t1, t2);

    // Invoke the start function of the module.
    let func = instance.get_func(&mut store, "_start")
        .expect("Unable to find function _start. ");
    let mut result = [];

    // Then run the remaining portion of the funciton.
    t1 = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let _r = func.call(&mut store, &[], &mut result);
    t2 = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    execution_time_after_migration = timespec_diff(t1, t2);

    // Now rerun the entire function for peace of mind.
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

    // Add the should_migrate function, again, it will never signal a pending migration.
    linker.func_wrap("host", "should_migrate", || { 0 } )
        .expect("func_wrap failed. ");

    // Add the restore_memory, which do nothing right now.
    linker.func_wrap("host", "restore_memory", || { } )
        .expect("func_wrap failed. ");

    // Compute the pre_instance time.
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

    // Effective execution time.
    t1 = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let _r = func.call(&mut store, &[], &mut result);
    t2 = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let execution_time = timespec_diff(t1, t2);

    // Finally, print all the acquired data:
    println!("Computation {}: ", benchmark.name);
    /* println!("  - pre_instantiation_time          : {:?}", pre_instantiation_time);
    println!("  - instantiation_time              : {:?}", instantiation_time);
    println!("  - execution_time_before_migration : {:?}", execution_time_before_migration);
    println!("  - get_checkpoint_memory_time      : {:?}", get_checkpoint_memory_time);
    println!("  - checkpoint_memory_copy_time     : {:?}", checkpoint_memory_copy_time);
    println!("  - get_main_memory_time            : {:?}", get_main_memory_time);
    println!("  - main_memory_copy_time           : {:?}", main_memory_copy_time);
    println!("  - pre_instantiation_time_after    : {:?}", pre_instantiation_time_after);
    println!("  - instantiation_time_after        : {:?}", instantiation_time_after);
    println!("  - execution_time_after_migration  : {:?}", execution_time_after_migration);
    println!("  - execution_time                  : {:?}", execution_time);
    println!("End. "); */
}

fn main() {

    // Get a few things from the command line.
    let args: Vec<String> = env::args().collect();
    let distributed_as_string = &args[1];
    let distributed = distributed_as_string.parse::<bool>().unwrap();
    let comp_as_string = &args[2];
    let comp = comp_as_string.parse::<usize>().unwrap();
    let reg_as_string = &args[3];
    let reg = reg_as_string.parse::<i32>().unwrap();

    print!("Experiment running with distributed = {}. ", distributed);

    let benchmarks = Benchmarks::new();

    // Compile all the computation anew.
    println!("Inserting the runtime procedures. ");
    for benchmark in &benchmarks.benchmarks {
        insert_checkpoint_and_restore(benchmark, distributed);
    }

    let path_to_wasm = "./wasm_migrate_eval_migration/migrating_bunch".to_string();
    let benchmark = &benchmarks.benchmarks[comp];

    // Comp. 6 -> Regions. 6
    // Comp. 1 -> Regions. 4

    run_and_migrate(benchmark, path_to_wasm, reg);

}
