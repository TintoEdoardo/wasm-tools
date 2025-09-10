<div align="center">
  <h1><code>wasm-migrate</code></h1>

<strong>By <a href="https://computecontinuum.eu/">Edoardo Tinto</a></strong>

  <p>
    <strong>wasm-migrate is a tool to enable a computation to migrate, virtually, from any runtime.</strong>
  </p>

</div>

<!-- .  -->


## Usage

Add `wasm-migrate` to your `Cargo.toml`:

## Features

* **checkpoint and restore:** injects checkpoint and restore procedures directly in the Wasm bytecode. 
* **migration:** two host imported functions are required to migrate. 
  ### Example

  ```rust
  use std::fs::File;
  use std::io::{Read, Write};
  use crate::WasmMigrate;

  fn inject_checkopint() -> Result<(), Box<dyn std::error::Error>> {
  
        let mut migration_injector: WasmMigrate = WasmMigrate::new();

        // Load the module Wasm bytecode.
        let mut file = File::open("./tests/gcd.wasm").unwrap();
        let mut wasm_bytes = Vec::new();
        let _ = file.read_to_end(&mut wasm_bytes);

        // Select a function index
        let func_index = 12;
        let modified_body = 
            migration_injector.compute(&wasm_bytes.clone(), func_index);

        // For demonstration, let's just write the modified body to a new file
        let mut output_file = 
            std::fs::OpenOptions::new()
                .write(true).create(true)
                .truncate(true)
                .open("./tests/gcd_with_checkpoints.wasm")?;
  
        let _ = output_file.write(modified_body.as_slice());
  
        Ok(())
    }
  ```



# License

This project is licensed under the Apache 2.0 license with the LLVM exception.
See [LICENSE](../../LICENSE) for more details.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license,
shall be licensed as above, without any additional terms or conditions.

### Special contribution

* Edoardo Tinto (Phd. student at UniPD)
