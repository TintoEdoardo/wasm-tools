use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::ops::Range;
use anyhow::Result;
use wasm_encoder::reencode::{Reencode, ReencodeComponent, RoundtripReencoder};
use wasm_encoder::{BlockType, CodeSection, DataCountSection, DataSection, ElementSection, Elements, EntityType, ExportKind, ExportSection, Function, FunctionSection, GlobalType, ImportSection, Instruction, MemArg, MemorySection, MemoryType, Module, ModuleSection, NestedComponentSection, TypeSection, ValType};
use wasm_mutate::module::map_type;
use wasmparser::{BinaryReader, ElementKind, Export, ExternalKind, FunctionBody, Operator, Parser, Payload};

#[derive(Eq, PartialEq, Debug, Clone)]
enum NodeType {
    CODE,
    BLOCK,
    LOOP,
    IF,
    ELSE,
}

enum NestingLevel {
    ONE,
}

/// To place the checkpoints in the computation we can use various
/// approaches, the one adopted here tries to build a tree, where each node
/// can be:
/// (1) A portion of code between blocks,
/// (2) A well-formed block.
/// (3) A loop block.
/// (4) An if block.
/// (5) An else block.
/// The idea is to add a checkpoint at each node end, at least in the most
/// basic version.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct TreeNode {
    index: usize,
    node_type: NodeType,
    start_inst: usize,
    end_inst: usize,
    outer: usize,
    // succ: Vec<usize>,
}

impl TreeNode {
    fn new(index: usize, node_type: NodeType, start_inst: usize) -> Self {
        TreeNode {
            index,
            node_type,
            start_inst,
            end_inst: start_inst,
            outer: start_inst,
            // succ: vec![],
        }
    }
}

fn opening_block(node_type: NodeType,
                 inst_index: usize,
                 stack: &mut Vec<usize>,
                 blocks: &mut Vec<TreeNode>) {
    // Create a new node.
    let index = blocks.len() + 1;
    let mut node = TreeNode::new(index, node_type, inst_index);

    // First, fill the field outer. Note that 0 means the portion of code
    // is not within any outer block, except from the function body.
    if let Some(&stack_top) = stack.last() {
        node.outer = stack_top;
    } else {
        node.outer = 0;
    }

    // Push the new node into the stack.
    stack.push(node.index);

    // Push the new node into blocks.
    blocks.push(node);
}

fn closig_block(inst_index: usize,
                stack: &mut Vec<usize>,
                blocks: &mut Vec<TreeNode>) {
    // Select the node to conclude.
    if let Some(block_index) = stack.pop() {
        let index = block_index - 1;
        let block = &mut blocks[index];
        block.end_inst = inst_index;
    };
}

fn close_preceeding_code_portion(inst_index: usize,
                                 stack: &mut Vec<usize>,
                                 blocks: &mut Vec<TreeNode>) {
    let prev_inst = inst_index - 1;
    closig_block(prev_inst, stack, blocks);
}

/// This is a representation of the modules defined within a Wasm file
/// (either a module or a component). This information wil be used to
/// inject the checkpoint and restore (C/R) procedures.
#[derive(Clone)]
struct ModuleData<'a> {
    /// Start function index.
    start_func: u32,

    /// Type section of the module.
    type_section: Option<TypeSection>,

    // Reader for types in the section.
    // type_section_reader: Option<TypeSectionReader<'a>>,

    /// A mapping from function index to type index.
    function_to_type: Vec<u32>,

    /// Import section of the module.
    import_section: Option<ImportSection>,

    /// Table section of the module.
    table_section: Option<wasm_encoder::TableSection>,
    
    /// Tag section of the module. 
    tag_section: Option<wasm_encoder::TagSection>,

    /// Global section of the module.
    global_section: Vec<wasm_encoder::GlobalType>,
    global_init_expr: Vec<wasm_encoder::ConstExpr>,

    /// Export section of the module.
    export_section: Option<ExportSection>,

    /// List of export.
    exports: Vec<Export<'a>>,

    /// Function section of the module.
    function_section: Option<FunctionSection>,
    
    /// Element section of the module with update index for imported function.
    element_section_with_cr: Option<ElementSection>,

    /// Element section as taken from the input bytecode.
    element_section: Option<ElementSection>,

    /// Content of the code section as a vector of FunctionBody.
    code_section: Option<wasm_encoder::CodeSection>,

    /// Individual body of functions.
    function_bodies: Vec<FunctionBody<'a>>,

    /// Data section of the module.
    data_section: Option<DataSection>,

    /// DataCount section of the module (just its 'count' field to be precise).
    data_count_section: Option<u32>,

    /// Memory section of the module.
    memory_section: Option<MemorySection>,

    /// Custom section of the module.
    custom_section: Vec<wasmparser::CustomSectionReader<'a>>,

    /// Vectors containing params, results and locals for each function.
    /// The index is that of the code section.
    params: Vec< Vec<ValType>>,
    results: Vec< Vec<ValType>>,
    locals: Vec< Vec<(u32, ValType)>>,
    globals: Vec<ValType>,

    /// The index of the checkpoint memory, which comes after all the other
    /// memories declared in the module.
    checkpoint_memory_index: u32,

    num_of_import_functions: u32,

    /// The BBs in the function with index function_index.
    blocks: Vec<TreeNode>,

    /// Sorted list of blocks ending with a checkpoint, from the one with the smallest
    /// index, to the one with the larger.
    checkpoint_list: Vec<usize>,

    /// A mapping from each checkpoint block in checkpoint_list to
    /// a list of variable indices.
    modified_vars_set: Vec<HashSet<u32>>,

    /// A mapping from each region in checkpoint_list to
    /// a list of live-in variable indices.
    live_in_set_vars: Vec<HashSet<u32>>,

    /// A mapping from each region in checkpoint_list to
    /// a list of live-out variable indices.
    live_out_set_vars: Vec<HashSet<u32>>,

    /// A mapping from each checkpoint block in checkpoint_list to
    /// a list of global indices.
    set_global_vars: Vec<HashSet<u32>>,

    /// A mapping from each region in checkpoint_list to
    /// a list of (read) global indices.
    get_global_vars: Vec<HashSet<u32>>,

    /// A mapping from each region in checkpoint_list to
    /// a list of live-out global indices.
    live_out_global_vars: Vec<HashSet<u32>>,

    /// This variable contains the index of the block from which the computation
    /// should start after a migration.
    resuming_block_var: Option<u32>,

    /// The memory address used to store the value of resuming_block_var,
    /// which is checkpointed as the local variables.
    resuming_block_addr: u64,

    /// A mapping from each variable index to its memory address in
    /// the checkpoint memory.
    checkpoint_var_memory_mapping: HashMap<u32, u64>,

    /// A mapping from each global index to its memory address in
    /// the checkpoint memory.
    checkpoint_global_memory_mapping: HashMap<u32, u64>,

    /// A mapping from each variable index to the type of the variable.
    // type_of_locals: HashMap<u32, ValType>,

    /// Mapping from each NodeType::CODE block to the instruction in it.
    insts_in_code_node: HashMap<u32, Vec<u32>>

}

impl<'a> ModuleData<'a> {
    fn new() -> ModuleData<'a> {
        Self {
            start_func: 0,
            type_section: None,
            // type_section_reader: None,
            function_to_type: vec![],
            import_section: None,
            table_section: None,
            tag_section: None,
            global_section: vec![],
            global_init_expr: vec![],
            export_section: None,
            exports: vec![],
            function_section: None,
            element_section_with_cr: None,
            element_section: None,
            code_section: None,
            function_bodies: vec![],
            data_section: None,
            data_count_section: None,
            memory_section: None,
            custom_section: vec![],
            params: vec![],
            results: vec![],
            locals: vec![],
            globals: vec![],
            checkpoint_memory_index: 0,
            num_of_import_functions: 0,
            blocks: vec![],
            checkpoint_list: vec![],
            modified_vars_set: vec![],
            live_in_set_vars: vec![],
            live_out_set_vars: vec![],
            set_global_vars: vec![],
            get_global_vars: vec![],
            live_out_global_vars: vec![],
            resuming_block_var: None,
            resuming_block_addr: 0,
            checkpoint_var_memory_mapping: Default::default(),
            checkpoint_global_memory_mapping: Default::default(),
            insts_in_code_node: Default::default(),
        }
    }
}

/// This is a representation of a component within a Wasm file.
#[derive(Clone)]
#[allow(dead_code)]
struct ComponentData<'a> {

    /// This flag is true if the current component is in reality a module.
    /// The reason for it is that we need this sort of empty component to
    /// use the same parser for components and modules.
    is_it_a_module: bool,

    /// The index of the component (which corresponds to the order
    /// in the binary).
    component_index :i32,

    /// Core modules defined within the component.
    core_modules: Vec<ModuleData<'a>>,

    /// Instance section.
    instance_section: Option<wasm_encoder::InstanceSection>,

    /// CoreType section.
    core_type_section: Option<wasm_encoder::CoreTypeSection>,

    /// Component section.
    component_section: Vec<ComponentData<'a>>,

    /// Component instance section.
    component_instance_section: Option<wasm_encoder::ComponentInstanceSection>,

    /// Component alias section.
    component_alias_section: Option<wasm_encoder::ComponentAliasSection>,

    /// Component type section.
    component_type_section: Option<wasm_encoder::ComponentTypeSection>,

    /// Component canonical section.
    component_canonical_section: Option<wasm_encoder::CanonicalFunctionSection>,

    /// Component start function.
    component_start_function: Option<wasmparser::ComponentStartFunction>,

    /// Component import section.
    component_import_section: Option<wasm_encoder::ComponentImportSection>,

    /// Component export section.
    component_export_section: Option<wasm_encoder::ComponentExportSection>,

}

impl<'a> ComponentData<'a> {
    #[allow(dead_code)]
    fn new() -> ComponentData<'a> {
        Self {
            is_it_a_module: false,
            component_index: 0,
            core_modules: vec![],
            instance_section: None,
            core_type_section: None,
            component_section: vec![],
            component_instance_section: None,
            component_alias_section: None,
            component_type_section: None,
            component_canonical_section: None,
            component_start_function: None,
            component_import_section: None,
            component_export_section: None,
        }
    }
}

#[derive(serde::Serialize)]
pub struct ComputationInfo {
    name: String,
    blocks: Vec<u32>,
    params_and_locals: Vec<u32>,
    checkpoints: Vec<u32>,
    live_out_vars: Vec<HashSet<u32>>,
}

#[cfg_attr(feature = "clap", derive(Parser))]
#[derive(Clone)]
pub struct WasmMigrate<'a> {
    /// The index of the component in which the module we want to insert
    /// C/R into is defined.
    pub component_index: i32,

    /// The index of the core module in which to insert the C/R procedures.
    pub core_module_index: i32,

    /// Index of the function where to inject the C/R procedures.
    pub func_index: i32,

    /// Components in the bytecode. If the bytecode does not include any components,
    /// this array will contain one empty ComponentData containing the module.
    components: Vec<(ComponentData<'a>, i32)>,
}

impl<'a> WasmMigrate<'a> {
    pub fn new() -> WasmMigrate<'a> {
        Self {
            component_index: 0,
            core_module_index: 0,
            func_index: 0,
            components: vec![],
        }
    }

    fn skip_locals(reader: &mut BinaryReader) -> wasmparser::Result<()> {
        let count = reader.read_var_u32()?;
        for _ in 0..count {
            reader.read_var_u32()?;
            reader.read::<wasmparser::ValType>()?;
        }
        Ok(())
    }

    /// Build a sort of CFG, represented as a vector in self.blocks, for the
    /// function with index func_index.
    fn build_block_tree(module_data: &mut ModuleData, func_index: usize) {
        let code_entry = module_data.function_bodies[func_index].clone();

        // Temporary stack to track opening and closing code portions.
        let mut stack: Vec<usize> = Vec::new();

        // This variable signifies that a code section is open.
        let mut code_section_is_open: bool = false;

        let mut binary_reader = code_entry.get_binary_reader();
        Self::skip_locals(&mut binary_reader).expect("skip_locals failed");

        while !binary_reader.eof() {
            let current_inst = binary_reader.current_position();
            let op = binary_reader.read_operator();
            // println!("inst {:?}: {:?}", current_inst, op);

            match op.unwrap() {
                Operator::Block { .. } => {
                    if code_section_is_open {
                        close_preceeding_code_portion(current_inst, &mut stack, &mut module_data.blocks);
                        code_section_is_open = false;
                    }
                    opening_block(NodeType::BLOCK, current_inst, &mut stack, &mut module_data.blocks);
                }
                Operator::Loop { .. } => {
                    if code_section_is_open {
                        close_preceeding_code_portion(current_inst, &mut stack, &mut module_data.blocks);
                        code_section_is_open = false;
                    }
                    opening_block(NodeType::LOOP, current_inst, &mut stack, &mut module_data.blocks);
                }
                Operator::If { .. } => {
                    if code_section_is_open {
                        close_preceeding_code_portion(current_inst, &mut stack, &mut module_data.blocks);
                        code_section_is_open = false;
                    }
                    opening_block(NodeType::IF, current_inst, &mut stack, &mut module_data.blocks);
                }
                Operator::Else => {
                    if code_section_is_open {
                        close_preceeding_code_portion(current_inst, &mut stack, &mut module_data.blocks);
                        code_section_is_open = false;
                    }
                    opening_block(NodeType::ELSE, current_inst, &mut stack, &mut module_data.blocks);
                }
                Operator::End => {
                    if code_section_is_open {
                        close_preceeding_code_portion(current_inst, &mut stack, &mut module_data.blocks);
                        code_section_is_open = false;
                    }
                    closig_block(current_inst, &mut stack, &mut module_data.blocks);
                }
                _ => {
                    if !code_section_is_open {
                        opening_block(NodeType::CODE, current_inst, &mut stack, &mut module_data.blocks);
                        let insts : Vec<u32> = Vec::from([current_inst as u32]);
                        let stack_top = *stack.last().unwrap() as u32;
                        module_data.insts_in_code_node.insert(stack_top, insts);
                        code_section_is_open = true;
                    } else {
                        let stack_top = *stack.last().unwrap() as u32;
                        module_data.insts_in_code_node
                            .get_mut(&stack_top).unwrap().push(current_inst as u32);
                    }
                }
            }
        }
        assert!(stack.is_empty());
    }

    /// Extract the locals of each local function.
    fn extract_locals(module_data: &mut ModuleData) {
        // Compute the list of locals for local functions.
        for function_body in &module_data.function_bodies {

            let mut locals_reader = function_body.get_locals_reader().unwrap();

            // Produce a list of the function local variables (which includes params).
            let mut local_count = 0;
            let local_variables: Vec<(u32, ValType)> = (0..locals_reader.get_count())
                .map(|_| {
                    let (count, ty) = locals_reader.read().unwrap();
                    local_count += count;
                    (count, map_type(ty).unwrap())
                })
                .collect::<Vec<(u32, ValType)>>();

            // Save the locals.
            module_data.locals.push(local_variables);
        }
    }

    /// List params and locals as a list of ValType. The func_index
    /// corresponds to a local function, NOT an import function.
    fn get_params_and_locals(module_data: ModuleData, func_index: usize) -> Vec<ValType> {
        let mut params_and_locals = Vec::new();
        let absolute_func_index = func_index + module_data.num_of_import_functions as usize;
        let func_type_index = module_data.function_to_type[absolute_func_index] as usize;
        params_and_locals.extend(module_data.params[func_type_index].clone());
        for (num, ty) in module_data.locals[func_index].clone() {
            for _ in 0..num {
                params_and_locals.push(ty);
            }
        }
        params_and_locals
    }

    /// Given a func index, assign to each variable a memory address where
    /// its checkpoint will be store.
    fn compute_var_to_mem_mapping(module_data: &mut ModuleData, func_index: usize)
    {
        // Prepare a list of params and locals.
        let params_and_locals =
            WasmMigrate::<'a>::get_params_and_locals(module_data.clone(), func_index);

        // Add resuming_block_var as a local of type I32.
        // This variable is not considered in params_and_locals, because it already
        // has an address associated (namely 0x0), stored in self.resuming_block_addr.
        module_data.locals[func_index].push((1, ValType::I32));

        // Save the index of resuming_block_var.
        // let mut locals_len = 0;
        // for (num, _ty) in module_data.locals[func_index].clone() {
        //     locals_len += num;
        // }
        // self.resuming_block_var = Some(locals_len - 1);
        module_data.resuming_block_var = Some(params_and_locals.len() as u32);

        // Map each variable to its memory address.
        let mut next_address = module_data.resuming_block_addr + 1;
        for local_index in 0..params_and_locals.len() {
            let ty = params_and_locals[local_index];
            let size: u64;
            match ty {
                ValType::I32 => {
                    size = 2;
                }
                ValType::I64 => {
                    size = 4;
                }
                ValType::F32 => {
                    size = 2;
                }
                ValType::F64 => {
                    size = 4;
                }
                ValType::V128 => {
                    size = 8;
                }
                ValType::Ref(_) => {
                    size = 2;
                }
            }
            // self.type_of_locals.insert(index, ty);
            module_data.checkpoint_var_memory_mapping.insert(local_index as u32, next_address);
            next_address += size;
        }

        // Then repeat the same operation for the globals.
        for global_index in 0..module_data.global_section.len() {
            let g = module_data.global_section[global_index].clone();
            let size: u64;
            match g.val_type {
                ValType::I32 => {
                    size = 2;
                }
                ValType::I64 => {
                    size = 4;
                }
                ValType::F32 => {
                    size = 2;
                }
                ValType::F64 => {
                    size = 4;
                }
                ValType::V128 => {
                    size = 8;
                }
                ValType::Ref(_) => {
                    size = 2;
                }
            }
            module_data.checkpoint_global_memory_mapping.insert(global_index as u32, next_address);
            next_address += size;
        }
    }

    /// Decide where to place checkpoints.
    fn generate_checkpoint_list(module_data: &mut ModuleData, _nesting_level: NestingLevel) {
        // In this version, each first-level block will be checkpointed.
        for block in &module_data.blocks {
            if block.outer == 0 {
                module_data.checkpoint_list.push(block.index);
            }
        }
    }

    /// Generate a mapping from each checkpoint to the list of variables
    /// modified since the previous checkpoint.
    fn generate_modified_var_list(module_data: &mut ModuleData, function_index: usize) {
        // Iterate over each checkpoint.
        for &checkpoint in &module_data.checkpoint_list {

            // Select the block corresponding this checkpoint.
            let block_index = checkpoint - 1;
            // self.checkpoint_list.iter().position(|&i| i == checkpoint).unwrap();
            let block = &module_data.blocks[block_index];

            let mut live_out_var: HashSet<u32> = HashSet::new();
            let mut set_global: HashSet<u32> = HashSet::new();

            let mut binary_reader =
                module_data.function_bodies[function_index].get_binary_reader();

            while !binary_reader.eof() {
                let current_inst = binary_reader.current_position();
                let op = binary_reader.read_operator();

                if current_inst < block.end_inst {
                    if current_inst >= block.start_inst {
                        match op.unwrap() {
                            Operator::LocalSet { local_index } => {
                                live_out_var.insert(local_index);
                            }
                            /* Operator::LocalGet { local_index } => {
                                live_out_var.insert(local_index);
                            } */
                            Operator::LocalTee { local_index } => {
                                live_out_var.insert(local_index);
                            }
                            Operator::GlobalSet { global_index } => {
                                set_global.insert(global_index);
                            }
                            _ => {}
                        }
                    }
                } else {
                    break;
                }
            }

            // TODO! This is temporary, using but necessary as we do not know when a global is set.
            for g_index in 0..module_data.global_section.len() {
                if module_data.global_section[g_index].mutable {
                    set_global.insert(g_index as u32);
                }
            }

            module_data.modified_vars_set.push(live_out_var);
            module_data.set_global_vars.push(set_global);
        }
    }

    /// Generate a mapping from each checkpoint to the list of live-in
    /// and live_out variables in the corresponding region.
    fn generate_live_var_list(module_data: &mut ModuleData, function_index: usize) {
        // Iterate over each checkpoint.
        for &checkpoint in module_data.checkpoint_list.iter().rev() {

            let mut live_in_var: HashSet<u32> = HashSet::new();
            let mut get_global: HashSet<u32> = HashSet::new();

            // Select the block corresponding this checkpoint.
            let block_index = checkpoint - 1;
            // self.checkpoint_list.iter().position(|&i| i == checkpoint).unwrap();
            let block = &module_data.blocks[block_index];

            let mut binary_reader =
                module_data.function_bodies[function_index].get_binary_reader();

            while !binary_reader.eof() {
                let current_inst = binary_reader.current_position();
                let op = binary_reader.read_operator();

                if current_inst < block.end_inst {
                    if current_inst >= block.start_inst {
                        match op.unwrap() {
                            Operator::LocalGet { local_index } => {
                                live_in_var.insert(local_index);
                            }
                            /* Operator::LocalGet { local_index } => {
                                live_out_var.insert(local_index);
                            } */
                            /* Operator::LocalTee { local_index } => {
                                live_in_var.insert(local_index);
                            } */
                            Operator::GlobalGet { global_index } => {
                                get_global.insert(global_index);
                            }
                            _ => {}
                        }
                    }
                } else {
                    break;
                }
            }

            module_data.live_in_set_vars.push(live_in_var.clone());
            module_data.get_global_vars.push(get_global.clone());
        }

        // Finally, reverse the two lists.
        module_data.live_in_set_vars.reverse();
        module_data.get_global_vars.reverse();

        // And then reset the first live-in set (which is empty).
        module_data.live_in_set_vars[0] = HashSet::new();
        module_data.get_global_vars[0] = HashSet::new();

        // Then compute the list of live-out var sets.
        // To do so, we need some additional data structures.

        // A mapping from each region to the list of variables modified
        // in this region or in any predecessor.
        let mut modified_vars_since_init = Vec::new();
        let mut cumulative_set : HashSet<u32> = HashSet::new();
        for set in &module_data.modified_vars_set {
            cumulative_set.extend(set);
            modified_vars_since_init.push(cumulative_set.clone());
        }

        // Same for globals.
        let mut modified_globals_since_init = Vec::new();
        let mut cumulative_global_set : HashSet<u32> = HashSet::new();
        for set in &module_data.set_global_vars {
            cumulative_global_set.extend(set);
            modified_globals_since_init.push(cumulative_global_set.clone());
        }

        // A mapping from each region to all the live-in variables in
        // any of its successor.
        let mut live_in_successor: Vec<HashSet<u32>> = Vec::new();
        let mut cumulative_set: HashSet<u32> = HashSet::new();
        // We compute the list from bottom to top.
        for i in (0..module_data.live_in_set_vars.len() - 1).rev() {
            // The last element has no successors.
            if i == module_data.live_in_set_vars.len() - 1 {
                cumulative_set = HashSet::new();
            }
            else {
                cumulative_set.extend(module_data.live_in_set_vars[i + 1].clone());
            }
            live_in_successor.push(cumulative_set.clone());
        }
        live_in_successor.reverse();

        // Same for globals.
        let mut live_in_global_successor: Vec<HashSet<u32>> = Vec::new();
        let mut cumulative_global_set: HashSet<u32> = HashSet::new();
        // We compute the list from bottom to top.
        for i in (0..module_data.get_global_vars.len() - 1).rev() {
            // The last element has no successors.
            if i == module_data.get_global_vars.len() - 1 {
                cumulative_global_set = HashSet::new();
            }
            else {
                cumulative_global_set.extend(module_data.get_global_vars[i + 1].clone());
            }
            live_in_global_successor.push(cumulative_global_set.clone());
        }
        live_in_global_successor.reverse();

        // Finally, we compute the live-out sets.
        for i in 0..module_data.checkpoint_list.len() {
            if i == module_data.checkpoint_list.len() - 1 {
                module_data.live_out_set_vars.push(HashSet::new());
                module_data.live_out_global_vars.push(HashSet::new());
            }
            else {
                let new_set =
                    modified_vars_since_init[i].clone()
                    .intersection(&live_in_successor[i])
                    .cloned()
                    .collect();
                module_data.live_out_set_vars.push(new_set);

                // And again the same thing for globals.
                let new_global_set : HashSet<u32> =
                        modified_globals_since_init[i].clone()
                        .intersection(&live_in_global_successor[i])
                        .cloned()
                        .collect();
                module_data.live_out_global_vars.push(new_global_set);
            }
        }

    }

    /// Load the value of the resuming_block_var, which can be
    /// in one of the following three states:
    /// 1. None, so it has not been initialized yet.
    /// 2. 0, the function is not resuming, the execution should proceed normally.
    /// 3. 1..n, the index of the checkpoint block from which resuming.
    /// This value is stored in the linear memory at a fixed address.
    fn initialize_resuming_block_var(module_data: ModuleData, index_restore_memory: u32) -> Vec<Instruction> {
        let mut insts : Vec<Instruction> = Vec::new();
        insts.push(Instruction::Block(BlockType::Empty));
        // Restore the linear memory.
        insts.push(Instruction::Call(index_restore_memory));
        // Push in the stack resuming_block_addr.
        insts.push(Instruction::I32Const(module_data.resuming_block_addr as i32));
        // Load from memory the value of the resuming block.
        insts.push(Instruction::I32Load(MemArg {
            offset: 0, align: 2, memory_index: module_data.checkpoint_memory_index,
        }));
        // Initialize the value of resuming_block_var.
        insts.push(Instruction::LocalSet(module_data.resuming_block_var.unwrap()));
        insts.push(Instruction::End);
        insts
    }

    /// Restore the modified vars of a block (restoring).
    fn resume_modified_vars(module_data: ModuleData, func_index: usize, block_index: usize, distributed: bool) -> Vec<Instruction> {

        // Get the index of the block with index block_index in checkpoint_index.
        let checkpoint_index =
            module_data.checkpoint_list.iter().position(|&i| i == block_index).unwrap();

        let mut modified_vars : Vec<u32>;
        let mut global_vars : Vec<u32>;
        if distributed {
            // Produce a vector with all the local variables to be checkpointed.
            modified_vars =
                module_data.modified_vars_set[checkpoint_index].iter().cloned().collect::<Vec<u32>>();

            // Produce a list with all the global variables to be checkpointed.
            global_vars =
                module_data.set_global_vars[checkpoint_index].iter().cloned().collect::<Vec<u32>>();
        }
        else {
            modified_vars =
                module_data.live_out_set_vars[checkpoint_index].iter().cloned().collect::<Vec<u32>>();

            global_vars =
                module_data.live_out_global_vars[checkpoint_index].iter().cloned().collect::<Vec<u32>>();
        }

        let mut insts = Vec::new();

        // Compute the list of all variables (locals and params).
        let params_and_locals =
            WasmMigrate::<'a>::get_params_and_locals(module_data.clone(), func_index);

        // Open a new block.
        insts.push(Instruction::Block(BlockType::Empty));

        // println!("Live out vars {:?}", live_out_vars);
        // println!("params_and_locals {:?}", params_and_locals);

        if distributed {
            // Check if we are resuming. So if the resuming_block_var is 0,
            // simply jump to the end of this block.
            insts.push(Instruction::LocalGet(module_data.resuming_block_var.unwrap()));
            insts.push(Instruction::I32Const(0));
            insts.push(Instruction::I32Eq);
            insts.push(Instruction::BrIf(0));
        }
        else {
            // Check if we are resuming. So if the resuming_block_var is 0,
            // (hence, we are not resuming) jump to the end of this block.
            insts.push(Instruction::LocalGet(module_data.resuming_block_var.unwrap()));
            insts.push(Instruction::I32Const(0));
            insts.push(Instruction::I32Eq);
            insts.push(Instruction::BrIf(0));
            // Then check if we are resuming from a later block. If so, jump
            // to the end of this block (without restoring anything).
            insts.push(Instruction::LocalGet(module_data.resuming_block_var.unwrap()));
            insts.push(Instruction::I32Const(block_index as i32));
            insts.push(Instruction::I32Ne);
            insts.push(Instruction::BrIf(0));
        }

        // Then insert the load instructions.
        while let Some(var) = modified_vars.pop() {
            let var_index = usize::try_from(var).unwrap();
            // Push the memory address of the var checkpoint to the stack top.
            insts.push(Instruction::I32Const(*module_data.checkpoint_var_memory_mapping.get(&var).unwrap() as i32));
            // Load from memory the value of the var.
            let mem_arg = MemArg {
                offset: 0,
                align: 2,
                memory_index: module_data.checkpoint_memory_index,
            };
            match params_and_locals[var_index] {
                ValType::I32 => insts.push(Instruction::I32Load(mem_arg)),
                ValType::I64 => insts.push(Instruction::I64Load(mem_arg)),
                ValType::F32 => insts.push(Instruction::F32Load(mem_arg)),
                ValType::F64 => insts.push(Instruction::F64Load(mem_arg)),
                ValType::V128 => insts.push(Instruction::V128Load(mem_arg)),
                ValType::Ref(_) => panic!("Local of type Ref, that should not happen. "),
            }
            // Set the corresponding variable.
            insts.push(Instruction::LocalSet(var));
        }

        // And then the load for the globals.
        while let Some(global) = global_vars.pop() {
            let global_index = usize::try_from(global).unwrap();
            insts.push(Instruction::I32Const(*module_data.checkpoint_global_memory_mapping.get(&global).unwrap() as i32));
            let mem_arg = MemArg {
                offset: 0,
                align: 2,
                memory_index: module_data.checkpoint_memory_index,
            };
            match module_data.globals[global_index] {
                ValType::I32 => insts.push(Instruction::I32Load(mem_arg)),
                ValType::I64 => insts.push(Instruction::I64Load(mem_arg)),
                ValType::F32 => insts.push(Instruction::F32Load(mem_arg)),
                ValType::F64 => insts.push(Instruction::F64Load(mem_arg)),
                ValType::V128 => insts.push(Instruction::V128Load(mem_arg)),
                ValType::Ref(_) => panic!("Global of type Ref, that should not happen. "),
            }
            // Set the corresponding variable.
            insts.push(Instruction::GlobalSet(global));
        }

        // Then close the block.
        insts.push(Instruction::End);

        insts
    }

    /// Store the live-out vars of a block (checkpointing).
    fn checkpoint_live_out_vars(module_data: ModuleData, func_index: usize, block_index: usize, distributed: bool) -> Vec<Instruction> {
        let checkpoint_index =
            module_data.checkpoint_list.iter().position(|&i| i == block_index).unwrap();
        let mut insts = Vec::new();

        let variable_set : HashSet<u32>;
        let global_set : HashSet<u32>;
        if distributed {
            variable_set = module_data.modified_vars_set[checkpoint_index].clone();
            global_set = module_data.set_global_vars[checkpoint_index].clone();
        }
        else {
            variable_set = module_data.live_out_set_vars[checkpoint_index].clone();
            global_set = module_data.live_out_global_vars[checkpoint_index].clone();
        }

        // Open a new block.
        insts.push(Instruction::Block(BlockType::Empty));

        for &var in variable_set.iter() {
            let var_index = usize::try_from(var).unwrap();

            // Push the memory address of the var to the stack top.
            insts.push(Instruction::I32Const(*module_data.checkpoint_var_memory_mapping.get(&var).unwrap() as i32));
            // Push the address in memory.
            insts.push(Instruction::LocalGet(var));

            // Store from memory the value of the var.
            let mem_arg = MemArg {
                offset: 0,
                align: 2,
                memory_index: module_data.checkpoint_memory_index,
            };

            // Compute the list of all variables (locals and params).
            let params_and_locals =
                WasmMigrate::<'a>::get_params_and_locals(module_data.clone(), func_index);

            // Then add the store instructions.
            match params_and_locals[var_index] {
                ValType::I32 => insts.push(Instruction::I32Store(mem_arg)),
                ValType::I64 => insts.push(Instruction::I64Store(mem_arg)),
                ValType::F32 => insts.push(Instruction::F32Store(mem_arg)),
                ValType::F64 => insts.push(Instruction::F64Store(mem_arg)),
                ValType::V128 => insts.push(Instruction::V128Store(mem_arg)),
                ValType::Ref(_) => panic!("Local of type Ref, that should not happen. "),
            }
        }

        for &global in global_set.iter() {
            let global_index = usize::try_from(global).unwrap();

            // Push the memory address of the var to the stack top.
            insts.push(Instruction::I32Const(*module_data.checkpoint_global_memory_mapping.get(&global).unwrap() as i32));
            // Push the address in memory.
            insts.push(Instruction::GlobalGet(global));

            // Store from memory the value of the var.
            let mem_arg = MemArg {
                offset: 0,
                align: 2,
                memory_index: module_data.checkpoint_memory_index,
            };

            // Compute the list of all variables (locals and params).
            let params_and_locals =
                WasmMigrate::<'a>::get_params_and_locals(module_data.clone(), func_index);

            // Then add the store instructions.
            match params_and_locals[global_index] {
                ValType::I32 => insts.push(Instruction::I32Store(mem_arg)),
                ValType::I64 => insts.push(Instruction::I64Store(mem_arg)),
                ValType::F32 => insts.push(Instruction::F32Store(mem_arg)),
                ValType::F64 => insts.push(Instruction::F64Store(mem_arg)),
                ValType::V128 => insts.push(Instruction::V128Store(mem_arg)),
                ValType::Ref(_) => panic!("Local of type Ref, that should not happen. "),
            }
        }

        // Record that we have completed this region.
        insts.push(Instruction::I32Const(module_data.resuming_block_addr as i32));
        insts.push(Instruction::I32Const(block_index as i32));
        insts.push(Instruction::I32Store(MemArg {
            offset: 0,
            align: 2,
            memory_index: module_data.checkpoint_memory_index,
        }));

        // Then close the block.
        insts.push(Instruction::End);

        insts
    }

    /// Check the resuming_block_var. If 0, the function is not resuming, and we
    /// can continue to run, otherwise:
    /// 1. If resuming_block_var == block_index, the function is resuming from this block,
    /// continue executing.
    /// 2. If resuming_block_var /= block_index, jump to the end of this block, the destination
    /// is further down.
    fn jump_to_end(module_data: ModuleData, block_index: usize) -> Vec<Instruction> {
        let mut insts = Vec::new();
        // Open a new block.
        insts.push(Instruction::Block(BlockType::Empty));
        // Get the value of resuming_block_var.
        insts.push(Instruction::LocalGet(module_data.resuming_block_var.unwrap()));
        // Push in the stack the index of the current block.
        insts.push(Instruction::I32Const(block_index as i32));
        // Check if they are not equal.
        insts.push(Instruction::I32Eq);
        // If they are equal, exit this block and resume the execution.
        insts.push(Instruction::BrIf(0));
        // Otherwise, check if we are resuming at all (hence see if
        // self.resuming_block_var is 0, if so, exit this block and
        // continue the execution).
        insts.push(Instruction::LocalGet(module_data.resuming_block_var.unwrap()));
        insts.push(Instruction::I32Const(0));
        insts.push(Instruction::I32Eq);
        insts.push(Instruction::BrIf(0));
        // Otherwise, jump to the end of the checkpoint region.
        insts.push(Instruction::Br(1));
        // Close this block.
        insts.push(Instruction::End);
        insts
    }

    fn print_block_instructions(module_data: ModuleData, block_index: usize, function_body: &FunctionBody<'a>)
        -> Vec<Instruction<'a>>
    {
        let block = &module_data.blocks[block_index - 1];
        let mut insts: Vec<Instruction> = Vec::new();
        let mut binary_reader = function_body.get_binary_reader();
        Self::skip_locals(&mut binary_reader).expect("skip_locals failed");

        while !binary_reader.eof() {
            let current_inst = binary_reader.current_position();
            let op = binary_reader.read_operator();
            // println!("current_inst: {:?} | op: {:?} ", current_inst, op);
            if current_inst <= block.end_inst {
                if current_inst >= block.start_inst {
                    match RoundtripReencoder.instruction(op.clone().unwrap()) {
                        Ok(Instruction::Call(index)) => {
                            let mut  new_index = index;
                            if index >= module_data.num_of_import_functions {
                                new_index += 2;
                            }
                            insts.push(Instruction::Call(new_index));
                        }
                        Ok(Instruction::ReturnCall(index)) => {
                            let mut  new_index = index;
                            if index >= module_data.num_of_import_functions {
                                new_index += 2;
                            }
                            insts.push(Instruction::ReturnCall(new_index));
                        }
                        Ok(instruction) => {
                            // println!("op: {:?}", op.unwrap());
                            // println!("inst: {:?}", instruction);
                            insts.push(instruction);
                        }
                        _ => {
                            panic!("Invalid roundtrip instruction");
                        }
                    }
                }
            } else {
                break;
            }
        }

        insts
    }

    /// In order to communicate to the module that it has to stop its execution
    /// a function is passed as import by the host.
    /// This function will be placed after the last imported function.
    /// Its type, at the end of the type section.
    fn update_function_indexes(module_data: ModuleData, function_body: &FunctionBody, locals: Vec<(u32, ValType)>)
    -> Function {
        // The new function body.
        let mut new_function_body = Function::new(locals);

        // Parse the old function body.
        let mut binary_reader = function_body.get_binary_reader();
        Self::skip_locals(&mut binary_reader).expect("skip_locals failed");
        while !binary_reader.eof() {
            let op = binary_reader.read_operator();
            // println!("op: {:?}", op.clone().unwrap());
            match RoundtripReencoder.instruction(op.clone().unwrap()) {
                Ok(Instruction::Call(index)) => {
                    let mut  new_index = index;
                    if index >= module_data.num_of_import_functions {
                        new_index += 2;
                    }
                    new_function_body.instruction(&Instruction::Call(new_index));
                }
                Ok(Instruction::ReturnCall(index)) => {
                    let mut  new_index = index;
                    if index >= module_data.num_of_import_functions {
                        new_index += 2;
                    }
                    new_function_body.instruction(&Instruction::ReturnCall(new_index));
                }
                // Note that we are not changing the addresses for indirect calls.
                // The reason is that those references have been already fixed in the
                // element section.
                Ok(instruction) => {
                    new_function_body.instruction(&instruction);
                }
                _ => {
                    panic!("Invalid roundtrip instruction");
                }
            }
        }
        new_function_body
    }

    /// Prepare the support data structures for encoding a module.
    fn pre_module_encoding(module_data: &mut ModuleData<'a>, func_index: usize, inject: bool)
    {
        // If the function that we want to C/R is here, then populate the
        // required data structures.
        if inject {
            // Extract locals, params and results for all functions.
            WasmMigrate::<'a>::extract_locals(module_data);

            // Build a tree out of the Wasm bytecode, first step to define regions.
            WasmMigrate::<'a>::build_block_tree(module_data, func_index);

            // Divide the function into regions.
            WasmMigrate::<'a>::generate_checkpoint_list(module_data, NestingLevel::ONE);

            // Compute the list of modified variables at each checkpoint.
            WasmMigrate::<'a>::generate_modified_var_list(module_data, func_index);

            // Compute the list of live-in vars for each region.
            WasmMigrate::<'a>::generate_live_var_list(module_data, func_index);

            // Assign a memory address to all the variables to be checkpointed.
            WasmMigrate::<'a>::compute_var_to_mem_mapping(module_data, func_index);
        }
    }

    /// Encode a module with checkpoint and restore procedures inserted.
    fn encode_module(module_data: &ModuleData<'a>, func_index: usize, inject: bool, distributed: bool) -> wasm_encoder::Module
    {
        // The module with the additional runtime procedures for checkpoint and restore.
        let mut module = Module::new();

        // ------------------------------- //
        //    Encode the type section.     //
        // ------------------------------- //
        let mut types = TypeSection::new();
        if let Some(type_section) = module_data.type_section.clone() {
            types = type_section;
        } else {
            let func_type_index = module_data.function_to_type[func_index];
            let params = module_data.params[func_type_index as usize].clone();
            let results = module_data.results[func_type_index as usize].clone();
            types.ty().function(params, results);
        }

        // Add the type of host-imported should_migrate.
        let index_should_migrate = types.len();
        let index_restore_memory = index_should_migrate + 1;
        if inject {
            types.ty().function(Vec::default(), [ValType::I32]);
            types.ty().function(Vec::default(), []);
        }

        // Then push the type section.
        module.section(&types);

        // ------------------------------- //
        //    Encode the import section.   //
        // ------------------------------- //
        match &module_data.import_section {
            None => {
                if inject {
                    let mut imports = ImportSection::new();
                    imports.import("host", "should_migrate",
                                   EntityType::Function(index_should_migrate));
                    imports.import("host", "restore_memory",
                                   EntityType::Function(index_restore_memory));
                    module.section(&imports);
                }
            }
            Some(import_section) => {
                let mut imports = import_section.clone();
                if inject {
                    imports.import("host", "should_migrate",
                                   EntityType::Function(index_should_migrate));
                    imports.import("host", "restore_memory",
                                   EntityType::Function(index_restore_memory));
                }
                module.section(&imports);
            }
        };

        // --------------------------------- //
        //    Encode the function section.   //
        // --------------------------------- //
        let mut functions = wasm_encoder::FunctionSection::new();
        if let Some(function_section) = module_data.function_section.clone() {
            functions = function_section;
            module.section(&functions);
        } else {
            if inject {
                let type_index = 0;
                functions.function(type_index);
                module.section(&functions);
            }
        }

        // ------------------------------ //
        //    Encode the table section.   //
        // ------------------------------ //
        // let mut tables = wasm_encoder::TableSection::new();
        if let Some(table_section) = &module_data.table_section {
            let tables = table_section.clone();
            module.section(&tables);
        }

        // ------------------------------- //
        //    Encode the memory section.   //
        // ------------------------------- //
        // Encode the linear memory.
        match &module_data.memory_section {
            Some(memory_section) => {
                let mut memories = memory_section.clone();
                if inject {
                    memories.memory(MemoryType {
                        minimum: 1,
                        maximum: None,
                        memory64: false,
                        shared: false,
                        page_size_log2: None,
                    });
                    memories.memory(MemoryType {
                        minimum: 2,
                        maximum: None,
                        memory64: false,
                        shared: false,
                        page_size_log2: None,
                    });
                    module.section(&memories);
                }
            }
            None => {
                let mut memories = MemorySection::new();
                if inject {
                    memories.memory(MemoryType {
                        minimum: 1,
                        maximum: None,
                        memory64: false,
                        shared: false,
                        page_size_log2: None,
                    });
                    memories.memory(MemoryType {
                        minimum: 2,
                        maximum: None,
                        memory64: false,
                        shared: false,
                        page_size_log2: None,
                    });
                    module.section(&memories);
                }
                else {
                    // Nothing to do.
                }
            }
        }

        // ------------------------------- //
        //     Encode the tag section.     //
        // ------------------------------- //
        /* match &module_data.tag_section {
            Some(tag_section) => {
                module.section(&tag_section.clone());
            }
            None => {
                // Nothing to be done.
            }
        } */

        // ------------------------------- //
        //    Encode the global section.   //
        // ------------------------------- //
        // TODO: Why are we not restoring the global section as is?
        let mut globals = wasm_encoder::GlobalSection::new();
        for i in 0..module_data.global_section.len() {
            globals.global(module_data.global_section[i],
                           &module_data.global_init_expr[i]);
        }
        module.section(&globals);

        // ------------------------------- //
        //    Encode the export section.   //
        // ------------------------------- //
        let mut exports = wasm_encoder::ExportSection::new();

        if inject {
            for export in module_data.exports.clone() {
                let mut offset = 0;
                if export.kind == ExternalKind::Func {
                    offset += 2;
                }
                exports.export(
                    export.name,
                    RoundtripReencoder.export_kind(export.kind),
                    RoundtripReencoder.external_index(export.kind, export.index + offset),
                );
            }
            exports.export("checkpoint_memory", ExportKind::Memory, module_data.checkpoint_memory_index);
            // exports.export("checkpoint_l_memory", ExportKind::Memory, module_data.checkpoint_memory_index + 1);
            module.section(&exports);
        }
        else {
            match &module_data.export_section {
                None => {
                    // Nothing to be done.
                }
                Some(export_section) => {
                    exports = export_section.clone();
                    module.section(&exports);
                }
            }
        }

        // ------------------------------- //
        //    Encode the start section.    //
        // ------------------------------- //
        // TODO

        // -------------------------------- //
        //    Encode the element section.   //
        // -------------------------------- //
        let elements: wasm_encoder::ElementSection;
        if inject {
            if let Some(element_section) = module_data.element_section_with_cr.clone() {
                elements = element_section;
                module.section(&elements);
            }
        }
        else {
            if let Some(element_section) = module_data.element_section.clone() {
                elements = element_section;
                module.section(&elements);
            }
        }

        // ------------------------------- //
        //  Encode the data count section. //
        // ------------------------------- //
        match &module_data.data_count_section {
            Some(count) => {
                module.section(&DataCountSection { count: *count });
            }
            None => {
                // Nothing to be done.
            }
        }

        // ----------------------------- //
        //    Encode the code section.   //
        // ----------------------------- //
        let mut codes = CodeSection::new();

        if inject {

            // First, encode the function before func_index.
            for i in 0..func_index {
                let func_body = module_data.function_bodies[i].clone();
                let locals = module_data.locals[i].clone();
                let func =
                    WasmMigrate::<'a>::update_function_indexes(module_data.clone(), &func_body, locals);
                codes.function(&func);
            }

            // Then encode the function at func_index.
            let locals = module_data.locals[func_index].clone();
            let mut f = Function::new(locals);

            if distributed {
                // We are performing distributed checkpoints.

                // Add the initial resuming instructions.
                let index_of_restore_memory = module_data.num_of_import_functions + 1;
                for inst in
                    WasmMigrate::<'a>::initialize_resuming_block_var(module_data.clone(), index_of_restore_memory) {
                    f.instruction(&inst);
                }

                // Add the checkpoint and restore instructions.
                let function_body =
                    module_data.function_bodies[func_index].clone();
                let index_of_should_migrate = module_data.num_of_import_functions;
                let mut previous_block_index = None;
                for block in module_data.blocks.iter() {
                    if module_data.checkpoint_list.contains(&block.index) {

                        // First, check whether this is the last region or the first. If so, there are
                        // a few things to mend.
                        let is_last = &block.index == module_data.checkpoint_list.last().unwrap();
                        let is_first = &block.index == module_data.checkpoint_list.first().unwrap();

                        // Place into a block.
                        if !is_last {
                            f.instruction(&Instruction::Block(BlockType::Empty));
                        }

                        // Add the resume instructions for this block predecessor.
                        if !is_first {
                            for inst in
                                WasmMigrate::<'a>::resume_modified_vars
                                    (module_data.clone(), func_index, previous_block_index.unwrap(), distributed) {
                                f.instruction(&inst);
                            }
                        }
                        previous_block_index = Some(block.index);

                        // Add the jump instructions for this block.
                        if !is_last {
                            for inst in
                                WasmMigrate::<'a>::jump_to_end(module_data.clone(), block.index) {
                                f.instruction(&inst);
                            }
                        }

                        // Load all the instructions in the inner blocks.
                        for inst in
                            WasmMigrate::<'a>::print_block_instructions(module_data.clone(), block.index, &function_body) {
                            f.instruction(&inst);
                        }

                        // At the end of the block, add the checkpoint insts only if this is
                        // not the last region in the function.
                        if !is_last {
                            for inst in
                                WasmMigrate::<'a>::checkpoint_live_out_vars(module_data.clone(), func_index, block.index, distributed) {
                                f.instruction(&inst);
                            }
                            // Then check whether the computation has to migrate.
                            f.instruction(&Instruction::Block(BlockType::Empty));
                            f.instruction(&Instruction::Call(index_of_should_migrate));
                            f.instruction(&Instruction::I32Const(0));
                            f.instruction(&Instruction::I32Eq);
                            f.instruction(&Instruction::BrIf(0));
                            f.instruction(&Instruction::Unreachable);
                            f.instruction(&Instruction::End);
                        }

                        // Then close the block.
                        if !is_last {
                            f.instruction(&Instruction::End);
                        }
                    }
                }
                f.instruction(&Instruction::End);
            }
            else {
                // We are performing centralized checkpoints.

                // Add the initial resuming instructions.
                let index_of_restore_memory = module_data.num_of_import_functions + 1;
                for inst in
                    WasmMigrate::<'a>::initialize_resuming_block_var(module_data.clone(), index_of_restore_memory) {
                    f.instruction(&inst);
                }

                // Add the checkpoint and restore instructions.
                let function_body =
                    module_data.function_bodies[func_index].clone();
                let index_of_should_migrate = module_data.num_of_import_functions;
                let mut previous_block_index = None;
                for block in module_data.blocks.iter() {
                    if module_data.checkpoint_list.contains(&block.index) {

                        // First, check whether this is the last region or the first. If so, there are
                        // a few things to mend.
                        let is_last = &block.index == module_data.checkpoint_list.last().unwrap();
                        let is_first = &block.index == module_data.checkpoint_list.first().unwrap();

                        // Place into a block.
                        if !is_last {
                            f.instruction(&Instruction::Block(BlockType::Empty));
                        }

                        // Add the resume instructions for this block predecessor.
                        if !is_first {
                            for inst in
                                WasmMigrate::<'a>::resume_modified_vars
                                    (module_data.clone(), func_index, previous_block_index.unwrap(), distributed) {
                                f.instruction(&inst);
                            }
                        }
                        previous_block_index = Some(block.index);

                        // Add the jump instructions for this block.
                        if !is_last {
                            for inst in
                                WasmMigrate::<'a>::jump_to_end(module_data.clone(), block.index) {
                                f.instruction(&inst);
                            }
                        }

                        // Load all the instructions in the inner blocks.
                        for inst in
                            WasmMigrate::<'a>::print_block_instructions(module_data.clone(), block.index, &function_body) {
                            f.instruction(&inst);
                        }

                        // At the end of the block, add the checkpoint insts only if this is
                        // not the last region in the function.
                        if !is_last {
                            for inst in
                                WasmMigrate::<'a>::checkpoint_live_out_vars(module_data.clone(), func_index, block.index, distributed) {
                                f.instruction(&inst);
                            }
                            // Then check whether the computation has to migrate.
                            f.instruction(&Instruction::Block(BlockType::Empty));
                            f.instruction(&Instruction::Call(index_of_should_migrate));
                            f.instruction(&Instruction::I32Const(0));
                            f.instruction(&Instruction::I32Eq);
                            f.instruction(&Instruction::BrIf(0));
                            f.instruction(&Instruction::Unreachable);
                            f.instruction(&Instruction::End);
                        }

                        // Then close the block.
                        if !is_last {
                            f.instruction(&Instruction::End);
                        }
                    }
                }
                f.instruction(&Instruction::End);
            }

            codes.function(&f);

            // Finally, encode the functions after func_index.
            for i in (func_index + 1)..module_data.function_bodies.len() {
                let func_body = module_data.function_bodies[i].clone();
                let locals = module_data.locals[i].clone();
                let func =
                    WasmMigrate::<'a>::update_function_indexes(module_data.clone(), &func_body, locals);
                codes.function(&func);
            }
            module.section(&codes);
        }
        else {
            // Encode all the functions in the code section of module_data.
            if let Some(code_section) = module_data.code_section.clone() {
                codes = code_section;
                module.section(&codes);
            }
        }

        // ----------------------------- //
        //    Encode the data section.   //
        // ----------------------------- //
        // Encode the previous data section.
        if let Some(data_section) = module_data.data_section.clone() {
            module.section(&data_section);
        }

        // ----------------------------- //
        //    Encode a custom section.   //
        // ----------------------------- //
        // Encode the previous data section.
        /* let mut reencoder = wasm_encoder::reencode::RoundtripReencoder;
        let custom_sections = module_data.custom_section.clone();
        for section in custom_sections {
            reencoder.parse_custom_section(&mut module, section.clone())
                .expect("Failed to parse custom section");
        } */

        // Extract the encoded Wasm bytes for this module.
        // let wasm_bytes = module.finish();

        module
    }

    /// Encode a component from ComponentData.
    /* fn encode_component(&mut self, component_data: ComponentData<'a>) -> wasm_encoder::Component
    {

        // ------------------------------- //
        //       Encode a component.       //
        // ------------------------------- //
        let mut component = wasm_encoder::Component::new();

        // ---------------------------------- //
        //  Encode a component start section. //
        // ---------------------------------- //
        match component_data.component_start_function {
            None => {
                // Nothing to be done.
            }
            Some(function) => {
                RoundtripReencoder.parse_component_start_section(&mut component, function)
                    .expect("Unable to parse the component start section");
            }
        }

        // --------------------------------- //
        //   Encode the core type section.   //
        // --------------------------------- //
        match component_data.core_type_section {
            None => {
                // Nothing to be done.
            }
            Some(types) => {
                let core_type_section = types.clone();
                component.section(&core_type_section);
            }
        }

        // ------------------------------- //
        //    Encode the type section.     //
        // ------------------------------- //
        match component_data.component_type_section {
            None => {
                // Nothing to be done.
            }
            Some(section) => {
                let component_type_section = section.clone();
                component.section(&component_type_section);
            }
        }

        // ------------------------------- //
        //   Encode the import section.    //
        // ------------------------------- //
        match component_data.component_import_section {
            None => {
                // Nothing to be done.
            }
            Some(section) => {
                let component_import_section= section.clone();
                component.section(&component_import_section);
            }
        }

        // ------------------------------- //
        //  Encode the instance section.   //
        // ------------------------------- //
        match component_data.component_instance_section {
            None => {
                // Nothing to be done.
            }
            Some(instances) => {
                let instance_section= instances.clone();
                component.section(&instance_section);
            }
        }

        // ------------------------------- //
        //    Encode the module section.   //
        // ------------------------------- //
        let mut module_index = 0;
        for module_data in component_data.core_modules {
            // Check if the function to checkpoint is here.
            let mut should_inject = false;
            if self.component_index == component_data.component_index
                && self.core_module_index == module_index {
                should_inject = true;
            }
            let module = WasmMigrate::<'a>::encode_module(module_data, self.func_index as usize, should_inject);

            // Add the section to the component.
            component.section(&wasm_encoder::ModuleSection(&module));

            // Update the current module index.
            module_index += 1;
        }

        // ------------------------------ //
        //  Encode the component section. //
        // ------------------------------ //
        for nested_component in component_data.component_section {
            // TODO: how to print nested components?
            let nested_comp = self.encode_component(nested_component);
            component.section(&NestedComponentSection(&nested_comp));
        }

        // ------------------------------- //
        //    Encode the alias section.    //
        // ------------------------------- //
        match component_data.component_alias_section {
            None => {
                // Nothing to be done.
            }
            Some(section) => {
                let component_alias_section= section.clone();
                component.section(&component_alias_section);
            }
        }

        // ------------------------------- //
        //   Encode the export section.    //
        // ------------------------------- //
        match component_data.component_export_section {
            None => {
                // Nothing to be done.
            }
            Some(section) => {
                let component_export_section= section.clone();
                component.section(&component_export_section);
            }
        }

        component

    } */

    /// Produce the final bytecode, with the additional C/R procedures.
    /* fn produce_final_bytecode(&mut self, component_data: ComponentData<'a>) -> Vec<u8> {

        let results: Vec<u8>;

        // First, determine if the bytecode actually contains any components,
        // or it is a module.
        if component_data.is_it_a_module {
            results =
                WasmMigrate::<'a>::encode_module(component_data.core_modules[0].clone(), self.func_index as usize, true)
                    .finish();
        }
        // Otherwise, parse the component (and recursively all the nested ones).
        else {
            results = self.encode_component(component_data).finish();
        }

        return results
    }
     */

    fn read_module_bytecode(module_data: &mut ModuleData<'a>,
                            parser: &wasmparser::Parser,
                            wasm_bytes: &'a [u8])
        -> Result<()>
    {

        for payload in parser.clone().parse_all(&wasm_bytes)
        {
            match payload? {
                Payload::Version { .. } => {
                    // Nothing to be done.
                }
                Payload::TypeSection(reader) => {
                    let mut new_type_section = TypeSection::new();
                    RoundtripReencoder.parse_type_section(&mut new_type_section, reader.clone())
                        .expect("Unable to read type section. ");
                    if !new_type_section.is_empty() {
                        module_data.type_section = Some(new_type_section);
                    }
                    // Save params and returns of each function.
                    for ty in reader.into_iter_err_on_gc_types() {
                        let ty = ty.unwrap();
                        let params = ty
                            .params()
                            .iter()
                            .copied()
                            .map(map_type)
                            .collect::<wasm_mutate::Result<Vec<_>, _>>().unwrap();
                        module_data.params.push(params.clone());
                        let results = ty
                            .results()
                            .iter()
                            .copied()
                            .map(map_type)
                            .collect::<wasm_mutate::Result<Vec<_>, _>>().unwrap();
                        module_data.results.push(results.clone());
                    }
                }
                Payload::ImportSection(reader) => {
                    let mut new_import_section = ImportSection::new();
                    RoundtripReencoder.parse_import_section(&mut new_import_section, reader.clone())
                        .expect("Unable to read import section. ");
                    if !new_import_section.is_empty() {
                        module_data.import_section = Some(new_import_section);
                    }
                    for ty in reader {
                        match ty.expect("Unable to parse import. ").ty {
                            wasmparser::TypeRef::Func(ty) => {
                                // Save imported functions
                                module_data.function_to_type.push(ty.clone());
                                module_data.num_of_import_functions += 1;
                            }
                            _ => { }
                        }
                    }
                }
                Payload::FunctionSection(reader) => {
                    let mut new_func_section = FunctionSection::new();
                    RoundtripReencoder.parse_function_section(&mut new_func_section, reader.clone())
                        .expect("Unable to read function section. ");
                    if !new_func_section.is_empty() {
                        module_data.function_section = Some(new_func_section);
                    }
                    for ty in reader {
                        module_data.function_to_type.push(ty.expect("Unable to read type. ").clone());
                    }
                }
                Payload::TableSection(reader) => {
                    let mut new_table_section = wasm_encoder::TableSection::new();
                    RoundtripReencoder.parse_table_section(&mut new_table_section, reader.clone())
                        .expect("Unable to read table section. ");
                    if !new_table_section.is_empty() {
                        module_data.table_section = Some(new_table_section);
                    }
                }
                Payload::MemorySection(reader) => {
                    let mut new_memory_section = MemorySection::new();
                    RoundtripReencoder.parse_memory_section(&mut new_memory_section, reader.clone())
                        .expect("Unable to read memory section. ");
                    if !new_memory_section.is_empty() {
                        module_data.memory_section = Some(new_memory_section);
                    }
                    // Update the index of the checkpoint memory (which will be placed at the
                    // end of the memory section).
                    for _memory in reader {
                        module_data.checkpoint_memory_index += 1;
                    }
                }
                Payload::TagSection(reader) => {
                    let mut new_tag_section = wasm_encoder::TagSection::new();
                    RoundtripReencoder.parse_tag_section(&mut new_tag_section, reader.clone())
                        .expect("Unable to read tag section. ");
                    if !new_tag_section.is_empty() {
                        module_data.tag_section = Some(new_tag_section);
                    }
                }
                Payload::GlobalSection(reader) => {
                    let mut new_global_section = wasm_encoder::GlobalSection::new();
                    RoundtripReencoder.parse_global_section(&mut new_global_section, reader.clone())
                        .expect("Unable to read global section. ");
                    for global in reader {
                        let global = global.unwrap();
                        module_data.global_section.push(GlobalType {
                            val_type: match global.ty.content_type {
                                wasmparser::ValType::I32 => {
                                    module_data.globals.push(ValType::I32);
                                    wasm_encoder::ValType::I32
                                },
                                wasmparser::ValType::I64 => {
                                    module_data.globals.push(ValType::I64);
                                    wasm_encoder::ValType::I64
                                },
                                wasmparser::ValType::F32 => {
                                    module_data.globals.push(ValType::F32);
                                    wasm_encoder::ValType::F32
                                },
                                wasmparser::ValType::F64 => {
                                    module_data.globals.push(ValType::F64);
                                    wasm_encoder::ValType::F64
                                },
                                wasmparser::ValType::V128 => {
                                    module_data.globals.push(ValType::V128);
                                    wasm_encoder::ValType::V128
                                },
                                wasmparser::ValType::Ref(_) => {
                                    unimplemented!("Not implemented yet")
                                },
                            },
                            mutable: global.ty.mutable,
                            shared: global.ty.shared,
                        });
                        let init_expr = RoundtripReencoder.const_expr(global.init_expr)
                            .expect("Missing initilization for global variable. ");
                        module_data.global_init_expr.push(init_expr);
                    }
                }
                Payload::ExportSection(reader) => {
                    let mut new_export_section = wasm_encoder::ExportSection::new();
                    RoundtripReencoder.parse_export_section(&mut new_export_section, reader.clone())
                        .expect("Unable to read export section. ");
                    if !new_export_section.is_empty() {
                        module_data.export_section = Some(new_export_section);
                    }
                    for export in reader {
                        module_data.exports.push(export.expect("Unable to parse export. "));
                    }
                }
                Payload::StartSection {func, .. } => {
                    module_data.start_func = func;
                }
                Payload::ElementSection(reader) => {
                    let mut original_element_section = wasm_encoder::ElementSection::new();
                    // Get the original element section.
                    RoundtripReencoder.parse_element_section(&mut original_element_section, reader.clone())
                        .expect("Unable to parse element section. ");
                    module_data.element_section = Some(original_element_section);

                    // The next portion of code is rather complex. The point is, increase by 1
                    // all reference to local functions in any element.
                    let mut new_element_section = wasm_encoder::ElementSection::new();
                    for element in reader {
                        let element = element.expect("Unable to parse element. ");
                        let items = element.items;
                        let items_content: Elements = match RoundtripReencoder.element_items(items).unwrap() {
                            Elements::Functions(elems) => {
                                let mut slice = Vec::new();
                                // e represents an index to a function.
                                for e in elems.to_vec() {
                                    let mut offset = 0;
                                    if e >= module_data.num_of_import_functions {
                                        offset += 2;
                                    }
                                    slice.push(e + offset);
                                }
                                Elements::Functions(Cow::from(slice))
                            }
                            Elements::Expressions(ty, el) => {
                                Elements::Expressions(ty, el)
                            }
                        };
                        match element.kind {
                            ElementKind::Passive => {
                                new_element_section.passive(items_content);
                            }
                            ElementKind::Active { table_index, offset_expr } => {
                                let offset = RoundtripReencoder.const_expr(offset_expr).unwrap();
                                new_element_section.active(table_index, &offset, items_content);
                            }
                            ElementKind::Declared => {
                                new_element_section.declared(items_content);
                            }
                        }
                    }
                    if !new_element_section.is_empty() {
                        module_data.element_section_with_cr = Some(new_element_section);
                    }
                }
                Payload::DataCountSection { count, .. } => {
                    module_data.data_count_section = Some(count);
                }
                Payload::DataSection(reader) => {
                    let mut new_data_section = DataSection::new();
                    RoundtripReencoder.parse_data_section(&mut new_data_section, reader.clone())
                        .expect("Unable to read data section. ");
                    if !new_data_section.is_empty() {
                        module_data.data_section = Some(new_data_section);
                    }
                }
                Payload::CodeSectionStart { range, .. } => {
                    let mut code_section = wasm_encoder::CodeSection::new();
                    let orig_offset = parser.clone().offset() as usize;
                    let get_original_section = |range: Range<usize>| {
                        wasm_bytes.get(range.start - orig_offset..range.end - orig_offset)
                            .expect("Unable to get original section from bytes. ")
                    };
                    let section = get_original_section(range.clone());
                    let reader = wasmparser::BinaryReader::new(section, range.start);
                    let section = wasmparser::CodeSectionReader::new(reader)?;
                    RoundtripReencoder.parse_code_section(&mut code_section, section)?;
                    module_data.code_section = Some(code_section);
                }
                Payload::CodeSectionEntry(body) => {
                    module_data.function_bodies.push(body.clone());
                }
                Payload::CustomSection(reader) => {
                    module_data.custom_section.push(reader.clone());
                }
                Payload::End(_) => {
                    // println!("End of module! ");
                }
                _ => {
                    println!("Something else was found while parsing a module. ");
                    // panic!("The current payload does not correspond to a module one. ");
                }
            }
        }
        Ok(())
    }

    fn elaborate_component_bytecode(component: &mut wasm_encoder::Component,
                                    func_index: usize,
                                    component_number: i32,
                                    parser: wasmparser::Parser,
                                    wasm_bytes: &'a [u8],
                                    target_component: i32,
                                    target_module: i32,
                                    distributed: bool)
        -> Result<()>
    {
        // Check if the target function is here.
        let mut is_target_component = false;
        if target_component == component_number {
            is_target_component = true;
        }

        let mut module_index = 0;

        for payload in parser.parse_all(&wasm_bytes)
        {
            match payload? {
                Payload::ModuleSection { parser, unchecked_range } => {
                    let mut module_data = ModuleData::new();
                    module_index += 1;
                    Self::read_module_bytecode(&mut module_data, &parser, &wasm_bytes[unchecked_range])
                        .expect("Unable to read the module section. ");
                    // Check if the target function is in this module.
                    let mut is_target_module = false;
                    if is_target_component && target_module == module_index {
                        is_target_module = true;
                    }
                    Self::pre_module_encoding(&mut module_data, func_index, is_target_module);
                    let module =
                        Self::encode_module(&mut module_data, func_index, is_target_module, distributed);
                    component.section(&ModuleSection(&module));
                }
                Payload::InstanceSection(reader) => {
                    let mut instance_section = wasm_encoder::InstanceSection::new();
                    RoundtripReencoder.parse_instance_section(&mut instance_section, reader)
                        .expect("Unable to parse instance section. ");
                    // self.components[current_component].0.instance_section = Some(instance_section);
                    component.section(&instance_section);
                }
                Payload::CoreTypeSection(reader) => {
                    let mut core_type_section = wasm_encoder::CoreTypeSection::new();
                    RoundtripReencoder.parse_core_type_section(&mut core_type_section, reader)
                        .expect("Unable to parse core section. ");
                    // self.components[current_component].0.core_type_section = Some(core_type_section);
                    component.section(&core_type_section);
                }
                Payload::ComponentSection { parser, unchecked_range } => {
                    let mut nested_component = wasm_encoder::Component::new();
                    Self::elaborate_component_bytecode(&mut nested_component,
                                                       func_index,
                                                       component_number + 1,
                                                       parser,
                                                       &wasm_bytes[unchecked_range],
                                                       target_component,
                                                       target_module,
                                                       distributed)
                        .expect("Unable to parse a nested component. ");
                    component.section(&NestedComponentSection(&nested_component));
                }
                Payload::ComponentInstanceSection(reader) => {
                    let mut component_instance_section = wasm_encoder::ComponentInstanceSection::new();
                    RoundtripReencoder.parse_component_instance_section(&mut component_instance_section, reader)
                        .expect("Unable to parse component instance section. ");
                    // self.components[current_component].0.component_instance_section = Some(component_instance_section);
                    component.section(&component_instance_section);
                }
                Payload::ComponentAliasSection(reader) => {
                    let mut component_alias_section = wasm_encoder::ComponentAliasSection::new();
                    RoundtripReencoder.parse_component_alias_section(&mut component_alias_section, reader)
                        .expect("Unable to parse component alias section. ");
                    // self.components[current_component].0.component_alias_section = Some(component_alias_section);
                    component.section(&component_alias_section);
                }
                Payload::ComponentTypeSection(reader) => {
                    let mut component_type_section = wasm_encoder::ComponentTypeSection::new();
                    RoundtripReencoder.parse_component_type_section(&mut component_type_section, reader)
                        .expect("Unable to parse component type section. ");
                    //self.components[current_component].0.component_type_section = Some(component_type_section);
                    component.section(&component_type_section);
                }
                Payload::ComponentCanonicalSection(reader) => {
                    let mut component_canonical_section = wasm_encoder::CanonicalFunctionSection::new();
                    RoundtripReencoder.parse_component_canonical_section(&mut component_canonical_section, reader)
                        .expect("Unable to parse component canonical section. ");
                    //self.components[current_component].0.component_canonical_section = Some(component_canonical_section);
                    component.section(&component_canonical_section);
                }
                Payload::ComponentStartSection { start, range: _ } => {
                    // self.components[current_component].0.component_start_function = Some(start);
                    RoundtripReencoder.parse_component_start_section(component, start)
                        .expect("Unable to parse the component start section");
                }
                Payload::ComponentImportSection(reader) => {
                    let mut component_import_section = wasm_encoder::ComponentImportSection::new();
                    RoundtripReencoder.parse_component_import_section(&mut component_import_section, reader)
                        .expect("Unable to parse component import section. ");
                    // self.components[current_component].0.component_import_section = Some(component_import_section);
                    component.section(&component_import_section);
                }
                Payload::ComponentExportSection(reader) => {
                    let mut component_export_section = wasm_encoder::ComponentExportSection::new();
                    RoundtripReencoder.parse_component_export_section(&mut component_export_section, reader)
                        .expect("Unable to parse component export section. ");
                    // self.components[current_component].0.component_export_section = Some(component_export_section);
                    component.section(&component_export_section);
                }
                Payload::CustomSection(reader) => {
                    let mut reencoder = wasm_encoder::reencode::RoundtripReencoder;
                    reencoder.parse_component_custom_section(component, reader)
                        .expect("Unable to parse the component custom section");
                }
                Payload::UnknownSection { .. } => {
                    wasm_mutate::ErrorKind::Unsupported("Unknown sections. ".to_string());
                }
                Payload::End(_) => {
                    /* if reading_a_module
                    {
                        // To reach this point, we are using the component model
                        // and the finalized module is within a component.
                        let component = &mut self.components[current_component].0;
                        component.core_modules.push(current_module.clone());
                        reading_a_module = false;
                    }
                    else
                    {
                        // If we are here, we have finish reading a component,
                        // so we move back to its parent.
                        match self.components[current_component] {
                            (_, -1) => {
                                // Do nothing, we have finished.
                            }
                            (_, i) => {
                                // Add the current component as nested one within its parent.
                                let nested_component = self.components[current_component].0.clone();
                                let parent: &mut ComponentData = &mut self.components[i as usize].0;
                                parent.component_section.push(nested_component);
                                // Then update the index of the current component, moving to its parent.
                                current_component = i as usize;
                            }
                        }
                    }*/
                }
                _ => {}
            }
        }
        Ok(())

    }

    /// Insert the instructions to perform checkpoint and restore.
    pub fn compute<'b>(&'a self, wasm_bytes: &'b [u8], distributed: bool) -> Vec<u8> where 'b: 'a {

        let parser = Parser::new(0);
        let result: Vec<u8>;
        /*
        for payload in parser.parse_all(&wasm_bytes) {
            match payload {
                Ok(Payload::StartSection {func, .. }) => {
                    self.module_data.start_func = func;
                }
                Ok(Payload::TypeSection(reader, ..)) => {
                    let mut new_type_section = TypeSection::new();
                    RoundtripReencoder.parse_type_section(&mut new_type_section, reader.clone())
                        .expect("Unable to read type section. ");
                    if !new_type_section.is_empty() {
                        self.module_data.type_section = Some(new_type_section);
                    }
                    // Save params and returns of each function.
                    for ty in reader.into_iter_err_on_gc_types() {
                        let ty = ty.unwrap();
                        let params = ty
                            .params()
                            .iter()
                            .copied()
                            .map(map_type)
                            .collect::<wasm_mutate::Result<Vec<_>, _>>().unwrap();
                        self.module_data.params.push(params.clone());
                        let results = ty
                            .results()
                            .iter()
                            .copied()
                            .map(map_type)
                            .collect::<wasm_mutate::Result<Vec<_>, _>>().unwrap();
                        self.module_data.results.push(results.clone());
                    }
                }
                Ok(Payload::ImportSection(reader)) => {
                    let mut new_import_section = ImportSection::new();
                    RoundtripReencoder.parse_import_section(&mut new_import_section, reader.clone())
                        .expect("Unable to read import section. ");
                    if !new_import_section.is_empty() {
                        self.module_data.import_section = Some(new_import_section);
                    }
                    for ty in reader {
                        match ty.expect("Unable to parse import. ").ty {
                            wasmparser::TypeRef::Func(ty) => {
                                // Save imported functions
                                self.module_data.function_to_type.push(ty.clone());
                                self.module_data.num_of_import_functions += 1;
                            }
                            _ => { }
                        }
                    }
                }
                Ok(Payload::TableSection(reader)) => {
                    let mut new_table_section = wasm_encoder::TableSection::new();
                    RoundtripReencoder.parse_table_section(&mut new_table_section, reader.clone())
                        .expect("Unable to read table section. ");
                    if !new_table_section.is_empty() {
                        self.module_data.table_section = Some(new_table_section);
                    }
                }
                Ok(Payload::GlobalSection(reader)) => {
                    let mut new_global_section = GlobalSection::new();
                    RoundtripReencoder.parse_global_section(&mut new_global_section, reader.clone())
                        .expect("Unable to read global section. ");
                    /* if !new_global_section.is_empty() {
                        self.module_data.global_section = Some(new_global_section);
                    } */
                    for global in reader {
                        let global = global.unwrap();
                        self.module_data.global_section.push(GlobalType {
                            val_type: match global.ty.content_type {
                                wasmparser::ValType::I32 => {
                                    self.module_data.globals.push(ValType::I32);
                                    wasm_encoder::ValType::I32
                                },
                                wasmparser::ValType::I64 => {
                                    self.module_data.globals.push(ValType::I64);
                                    wasm_encoder::ValType::I64
                                },
                                wasmparser::ValType::F32 => {
                                    self.module_data.globals.push(ValType::F32);
                                    wasm_encoder::ValType::F32
                                },
                                wasmparser::ValType::F64 => {
                                    self.module_data.globals.push(ValType::F64);
                                    wasm_encoder::ValType::F64
                                },
                                wasmparser::ValType::V128 => {
                                    self.module_data.globals.push(ValType::V128);
                                    wasm_encoder::ValType::V128
                                },
                                wasmparser::ValType::Ref(_) => {
                                    unimplemented!("Not implemented yet")
                                },
                            },
                            mutable: global.ty.mutable,
                            shared: global.ty.shared,
                        });
                        let init_expr = RoundtripReencoder.const_expr(global.init_expr)
                            .expect("Missing initilization for global variable. ");
                        self.module_data.global_init_expr.push(init_expr);
                    }
                }
                Ok(Payload::ExportSection(reader)) => {
                    let mut new_export_section = ExportSection::new();
                    RoundtripReencoder.parse_export_section(&mut new_export_section, reader.clone())
                        .expect("Unable to read export section. ");
                    if !new_export_section.is_empty() {
                        self.module_data.export_section = Some(new_export_section);
                    }
                    for export in reader {
                        self.module_data.exports.push(export.expect("Unable to parse export. "));
                    }
                }
                Ok(Payload::FunctionSection(reader)) => {
                    let mut new_func_section = FunctionSection::new();
                    RoundtripReencoder.parse_function_section(&mut new_func_section, reader.clone())
                        .expect("Unable to read function section. ");
                    if !new_func_section.is_empty() {
                        self.module_data.function_section = Some(new_func_section);
                    }
                    for ty in reader {
                        self.module_data.function_to_type.push(ty.expect("Unable to read type. ").clone());
                    }
                }
                Ok(Payload::ElementSection(reader)) => {
                    let mut new_element_section = wasm_encoder::ElementSection::new();
                    /* RoundtripReencoder.parse_element_section(&mut new_element_section, reader.clone())
                        .expect("Unable to read element section. "); */
                    // The next portion of code is rather complex. The point is, increase by 1
                    // all references to local functions in any element.
                    for element in reader {
                        let element = element.expect("Unable to parse element. ");
                        let items = element.items;
                        let items_content: Elements = match RoundtripReencoder.element_items(items).unwrap() {
                            Elements::Functions(elems) => {
                                let mut slice = Vec::new();
                                // e represents an index to a function.
                                for e in elems.to_vec() {
                                    let mut offset = 0;
                                    if e >= self.module_data.num_of_import_functions {
                                        offset += 2;
                                    }
                                    slice.push(e + offset);
                                }
                                Elements::Functions(Cow::from(slice))
                            }
                            Elements::Expressions(ty, el) => {
                                Elements::Expressions(ty, el)
                            }
                        };
                        match element.kind {
                            ElementKind::Passive => {
                                new_element_section.passive(items_content);
                            }
                            ElementKind::Active { table_index, offset_expr } => {
                                let offset = RoundtripReencoder.const_expr(offset_expr).unwrap();
                                new_element_section.active(table_index, &offset, items_content);
                            }
                            ElementKind::Declared => {
                                new_element_section.declared(items_content);
                            }
                        }
                    }
                    if !new_element_section.is_empty() {
                        self.module_data.element_section = Some(new_element_section);
                    }
                }
                Ok(Payload::CodeSectionEntry(body)) => {
                    self.module_data.code_section.push(body.clone());
                }
                Ok(Payload::DataSection(data)) => {
                    let mut new_data_section = DataSection::new();
                    RoundtripReencoder.parse_data_section(&mut new_data_section, data.clone())
                        .expect("Unable to read data section. ");
                    if !new_data_section.is_empty() {
                        self.module_data.data_section = Some(new_data_section);
                    }
                }
                Ok(Payload::MemorySection(reader)) => {
                    let mut new_memory_section = MemorySection::new();
                    RoundtripReencoder.parse_memory_section(&mut new_memory_section, reader.clone())
                        .expect("Unable to read memory section. ");
                    if !new_memory_section.is_empty() {
                        self.module_data.memory_section = Some(new_memory_section);
                    }
                    // Update the index of the checkpoint memory (which will be placed at the
                    // end of the memory section).
                    for _memory in reader {
                        self.checkpoint_memory_index += 1;
                        // println!("checkpoint memory index: {}", self.checkpoint_memory_index);
                    }
                }
                // Sections for WebAssembly components
                Ok(Payload::ComponentSection { parser, unchecked_range }) => {
                    let mut component_section = wasm_encoder::Component::new();
                    RoundtripReencoder.parse_component(&mut component_section, parser, &wasm_bytes[unchecked_range])
                        .expect("Unable to parse component section. ");
                    self.module_data.component_section = Some(component_section);
                }
                Ok(Payload::ModuleSection {parser, unchecked_range}) => {
                    let mut new_core_module = wasm_encoder::Module::new();
                    RoundtripReencoder.parse_core_module(&mut new_core_module, parser.clone(), &wasm_bytes[unchecked_range])
                        .expect("Unable to parse core module. ");
                    self.module_data.module_section = Some(new_core_module);
                }
                Ok(Payload::InstanceSection(reader)) => {
                    let mut new_instance_section = InstanceSection::new();
                    RoundtripReencoder.parse_instance_section(&mut new_instance_section, reader.clone())
                        .expect("Unable to parse instance section. ");
                    if !new_instance_section.is_empty() {
                        self.module_data.instance_section = Some(new_instance_section);
                    }
                }
                Ok(Payload::CoreTypeSection(reader)) => {
                    let mut new_core_type_section = wasm_encoder::CoreTypeSection::new();
                    RoundtripReencoder.parse_core_type_section(&mut new_core_type_section, reader.clone())
                    .expect("Unable to parse core type section. ");
                    if !new_core_type_section.is_empty() {
                        self.module_data.core_type_section = Some(new_core_type_section);
                    }
                }
                Ok(Payload::ComponentInstanceSection(reader)) => {
                    let mut new_component_instance_section = wasm_encoder::ComponentInstanceSection::new();
                    RoundtripReencoder.parse_component_instance_section(&mut new_component_instance_section, reader.clone())
                    .expect("Unable to parse instance section. ");
                    if !new_component_instance_section.is_empty() {
                        self.module_data.component_instance_section = Some(new_component_instance_section);
                    }
                }
                Ok(Payload::ComponentAliasSection(reader)) => {
                    let mut new_component_alias_section = wasm_encoder::ComponentAliasSection::new();
                    RoundtripReencoder.parse_component_alias_section(&mut new_component_alias_section, reader.clone())
                    .expect("Unable to parse instance section. ");
                    if !new_component_alias_section.is_empty() {
                        self.module_data.component_alias_section = Some(new_component_alias_section);
                    }
                }
                Ok(Payload::ComponentTypeSection(reader)) => {
                    let mut new_component_type_section = wasm_encoder::ComponentTypeSection::new();
                    RoundtripReencoder.parse_component_type_section(&mut new_component_type_section, reader.clone())
                    .expect("Unable to parse instance section. ");
                    if !new_component_type_section.is_empty() {
                        self.module_data.component_type_section = Some(new_component_type_section);
                    }
                }
                Ok(Payload::ComponentStartSection { start, .. }) => {
                    let mut component = wasm_encoder::Component::new();
                    RoundtripReencoder.parse_component_start_section(&mut component, start).unwrap();
                    // self.module_data.component_section = Some(component);
                }
                Ok(Payload::ComponentImportSection(reader)) => {
                    let mut new_component_import_section = wasm_encoder::ComponentImportSection::new();
                    RoundtripReencoder.parse_component_import_section(&mut new_component_import_section, reader.clone())
                    .expect("Unable to parse instance section. ");
                    if !new_component_import_section.is_empty() {
                        self.module_data.component_import_section = Some(new_component_import_section);
                    }
                }
                Ok(Payload::ComponentExportSection(reader)) => {
                    let mut new_component_export_section = wasm_encoder::ComponentExportSection::new();
                    RoundtripReencoder.parse_component_export_section(&mut new_component_export_section, reader.clone())
                    .expect("Unable to parse instance section. ");
                    if !new_component_export_section.is_empty() {
                        self.module_data.component_export_section = Some(new_component_export_section);
                    }
                }
                _ => {}
            }
        }
        */

        let target_component = self.component_index;
        let target_module = self.core_module_index;
        let func_index = self.func_index as usize;
        // let target_module_data : ModuleData;

        if wasmparser::Parser::is_component(wasm_bytes) {
            let mut component = wasm_encoder::Component::new();
            Self::elaborate_component_bytecode(&mut component,
                                               func_index,
                                               0,
                                               parser,
                                               wasm_bytes,
                                               target_component,
                                               target_module,
                                               distributed)
                .expect("Inside 'compute': unable to elaborate a component. ");
            result = component.finish();
        }
        else {
            let mut module_data = ModuleData::new();
            Self::read_module_bytecode(&mut module_data, &parser, wasm_bytes)
                .expect("Inside 'compute': unable to elaborate a module. ");
            Self::pre_module_encoding(&mut module_data, func_index, true);
            let module =
                Self::encode_module(&mut module_data, func_index, true, distributed);
            result = module.finish();
        }

        /*
        // Parse the module bytecode.
        self.read_bytecode(wasm_bytes).expect("Unable to read the Wasm bytecode. ");

        // Select the target module.
        let target_component: ComponentData = self.components[target_component].0.clone();
        self.target_module = target_component.core_modules[target_module].clone();

        // Extract locals, params and results for all functions.
        self.extract_locals();

        // Build a tree out of the Wasm bytecode, first step to define regions.
        self.build_block_tree();

        // Divide the function into regions.
        self.generate_checkpoint_list(NestingLevel::ONE);

        // Compute the list of live-out variables to checkpoint.
        self.generate_live_out_var_list();

        // Assign a memory address to all the variables to be checkpointed.
        self.compute_var_to_mem_mapping();

        self.produce_final_bytecode(self.components[0].0.clone())
         */

        result

    }


    /// Print relevant information for evaluation purposes.
    pub fn print_module_info(&self, func_index: u32, name: String) -> ComputationInfo {
        let component = &self.components[self.component_index as usize].0;
        let module = &component.core_modules[self.core_module_index as usize];
        let blocks = &module.blocks;
        let comp_info = ComputationInfo {
            name,
            blocks: {
                let mut block_list = vec![];
                for b in blocks {
                    block_list.push(b.index as u32);
                }
                block_list
            },
            params_and_locals: {
                let mut params_and_locals = vec![];
                for l in Self::get_params_and_locals(module.clone(), func_index as usize) {
                    match l {
                        ValType::I32 => { params_and_locals.push(4); }
                        ValType::I64 => { params_and_locals.push(8); }
                        ValType::F32 => { params_and_locals.push(4); }
                        ValType::F64 => { params_and_locals.push(8); }
                        ValType::V128 => { params_and_locals.push(16); }
                        ValType::Ref(_) => { unimplemented!("Not implemented yet. "); }
                    };
                }
                params_and_locals
            },
            checkpoints: {
                let mut checkpoints = vec![];
                for c in module.checkpoint_list.clone() {
                    checkpoints.push(c as u32);
                }
                checkpoints
            },
            live_out_vars: {
                let mut live_out_vars = vec![];
                for l in &module.modified_vars_set.clone() {
                    live_out_vars.push(l.clone());
                }
                live_out_vars
            }
        };

        comp_info
    }
}


#[cfg(test)]
mod tests {
    use crate::WasmMigrate;
    use std::fs::File;
    use std::io::{Read, Write};

    #[test]
    fn add_checkpoint_at() -> Result<(), Box<dyn std::error::Error>> {
        let mut migration_injector: WasmMigrate = WasmMigrate::new();

        // Load the module Wasm bytecode.
        // let mut file = File::open("./tests/gcd.wasm").unwrap();
        let mut file = File::open("./tests/trajectory_tracker.wasm").unwrap();
        //let mut file = File::open("./tests/classification-component-onnx.wasm").unwrap();
        let mut wasm_bytes = Vec::new();
        let _ = file.read_to_end(&mut wasm_bytes);

        // Configure the injector.
        migration_injector.component_index = 0;
        migration_injector.core_module_index = 0;
        migration_injector.func_index = 1;

        let distributed = true;

        let modified_body =
            migration_injector.compute(&wasm_bytes.clone(), distributed);

        // For demonstration, let's just write the modified body to a new file
        let mut output_file =
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                //.open("./tests/classification-with-cr.wasm")?;
                .open("./tests/trajectory_tracker_cr.wasm")?;
                // .open("./tests/gcd_with_checkpoints.wasm")?;

        let _ = output_file.write(modified_body.as_slice());

        Ok(())
    }
}