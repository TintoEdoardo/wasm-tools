use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use wasmparser::{Operator, FunctionBody, Parser, Payload, BinaryReader, Export, ExternalKind, ElementKind};
use wasm_encoder::{BlockType, CodeSection, DataSection, ElementSection, Elements, EntityType, ExportKind, ExportSection, Function, FunctionSection, GlobalSection, GlobalType, ImportSection, Instruction, MemArg, MemorySection, MemoryType, Module, StartSection, TypeSection, ValType};
use wasm_encoder::reencode::{Reencode, RoundtripReencoder};
use wasm_mutate::module::map_type;

#[derive(Eq, PartialEq, Debug)]
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
/// approach, the one adopted here tries to build a tree, where each node
/// can be:
/// (1) A portion of code between blocks,
/// (2) A well-formed block.
/// (3) A loop block.
/// (4) An if block.
/// (5) An else block.
/// The idea is to add checkpoint at each node end, at least in the most
/// basic version.
#[derive(Debug)]
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

    /// Global section of the module.
    global_section: Vec<wasm_encoder::GlobalType>,
    global_init_expr: Vec<wasm_encoder::ConstExpr>,

    /// Export section of the module.
    export_section: Option<ExportSection>,

    /// List of export.
    exports: Vec<Export<'a>>,

    /// Function section of the module.
    function_section: Option<FunctionSection>,
    
    /// Element section of the module. 
    element_section: Option<ElementSection>,

    /// Content of the code section as a vector of FunctionBody.
    /// In this form, individual functions can be modified.
    code_section: Vec<FunctionBody<'a>>,

    /// Data section of the module.
    data_section: Option<DataSection>,

    /// Memory section of the module.
    memory_section: Option<MemorySection>,

    /// Vectors containing params, results and locals for each function.
    /// The index is that of the code section.
    params: Vec< Vec<ValType>>,
    results: Vec< Vec<ValType>>,
    locals: Vec< Vec<(u32, ValType)>>,
    globals: Vec<ValType>,

    num_of_import_functions: u32,
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
            global_section: vec![],
            global_init_expr: vec![],
            export_section: None,
            exports: vec![],
            function_section: None,
            element_section: None,
            code_section: vec![],
            data_section: None,
            memory_section: None,
            params: vec![],
            results: vec![],
            locals: vec![],
            globals: vec![],
            num_of_import_functions: 0,
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

pub struct WasmMigrate<'a> {
    blocks: Vec<TreeNode>,

    /// Sorted list of blocks ending with a checkpoint, from the one with the smallest
    /// index, to the one with the larger.
    checkpoint_list: Vec<usize>,

    /// A mapping from each checkpoint block in checkpoint_list to
    /// a list of variable indices.
    live_out_set_vars: Vec<HashSet<u32>>,

    /// A mapping from each checkpoint block in checkpoint_list to
    /// a list of global indices.
    set_global_vars: Vec<HashSet<u32>>,

    /// This variable contains the index of the block from which the computation
    /// should start after a migration.
    resuming_block_var: Option<u32>,

    /// The memory address used to store the value of resuming_block_var,
    /// which is checkpointed as the local variables.
    resuming_block_addr: u64,

    /// The index of the checkpoint memory. Notice that the multi-memory
    /// feature is required.
    checkpoint_memory_index: u32,

    /// A mapping from each variable index to its memory address in
    /// the checkpoint memory.
    checkpoint_var_memory_mapping: HashMap<u32, u64>,

    /// A mapping from each global index to its memory address in
    /// the checkpoint memory.
    checkpoint_global_memory_mapping: HashMap<u32, u64>,

    /// A mapping from each variable index to the type of the variable.
    // type_of_locals: HashMap<u32, ValType>,

    /// Mapping from each NodeType::CODE block to the instruction in it.
    insts_in_code_node: HashMap<u32, Vec<u32>>,

    /* /// Parameters, results and locals of the current function.
    params: Vec<ValType>,
    results: Vec<ValType>,
    locals: Vec<ValType>, */

    /// Information about the module being manipulated.
    module_data: ModuleData<'a>,
}

impl<'a> WasmMigrate<'a> {
    pub fn new() -> WasmMigrate<'a> {
        Self {
            blocks: vec![],
            checkpoint_list: vec![],
            live_out_set_vars: vec![],
            set_global_vars: vec![],
            resuming_block_var: Some(0),
            resuming_block_addr: 0,
            checkpoint_memory_index: 0,
            checkpoint_var_memory_mapping: HashMap::new(),
            // type_of_locals: Default::default(),
            checkpoint_global_memory_mapping: HashMap::new(),
            insts_in_code_node: Default::default(),
            /*
            params: vec![],
            results: vec![],
            locals: vec![],
             */
            module_data: ModuleData::new(),
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
    fn build_block_tree(&mut self, func_index: u32) {
        let func_index = usize::try_from(func_index).unwrap();
        let code_entry = self.module_data.code_section[func_index].clone();

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
                        close_preceeding_code_portion(current_inst, &mut stack, &mut self.blocks);
                        code_section_is_open = false;
                    }
                    opening_block(NodeType::BLOCK, current_inst, &mut stack, &mut self.blocks);
                }
                Operator::Loop { .. } => {
                    if code_section_is_open {
                        close_preceeding_code_portion(current_inst, &mut stack, &mut self.blocks);
                        code_section_is_open = false;
                    }
                    opening_block(NodeType::LOOP, current_inst, &mut stack, &mut self.blocks);
                }
                Operator::If { .. } => {
                    if code_section_is_open {
                        close_preceeding_code_portion(current_inst, &mut stack, &mut self.blocks);
                        code_section_is_open = false;
                    }
                    opening_block(NodeType::IF, current_inst, &mut stack, &mut self.blocks);
                }
                Operator::Else => {
                    if code_section_is_open {
                        close_preceeding_code_portion(current_inst, &mut stack, &mut self.blocks);
                        code_section_is_open = false;
                    }
                    opening_block(NodeType::ELSE, current_inst, &mut stack, &mut self.blocks);
                }
                Operator::End => {
                    if code_section_is_open {
                        close_preceeding_code_portion(current_inst, &mut stack, &mut self.blocks);
                        code_section_is_open = false;
                    }
                    closig_block(current_inst, &mut stack, &mut self.blocks);
                }
                _ => {
                    if !code_section_is_open {
                        opening_block(NodeType::CODE, current_inst, &mut stack, &mut self.blocks);
                        let insts : Vec<u32> = Vec::from([current_inst as u32]);
                        let stack_top = *stack.last().unwrap() as u32;
                        self.insts_in_code_node.insert(stack_top, insts);
                        code_section_is_open = true;
                    } else {
                        let stack_top = *stack.last().unwrap() as u32;
                        self.insts_in_code_node
                            .get_mut(&stack_top).unwrap().push(current_inst as u32);
                    }
                }
            }
        }
        /* for block in &self.blocks {
            println!("Block {}, insts {}-{}, ty {:?}, outer {} ", block.index, block.start_inst, block.end_inst, block.node_type, block.outer);
        } */
        assert!(stack.is_empty());
    }

    /// Extract the locals of each local function.
    fn extract_locals(&mut self) {
        /*
        // Imported functions have no locals, so add empty list
        // for them.
        for _i in 0..self.module_data.num_of_import_functions {
            self.module_data.locals.push(Vec::default());
        }

        // Leave a space for the host-imported should_migrate function.
        self.module_data.locals.push(Vec::default()); */

        // Then compute the list of locals for local functions.
        for function_body in &self.module_data.code_section {

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
            self.module_data.locals.push(local_variables);
            /* for (num, ty) in local_variables {
                for _n in 0..num {
                    self.locals.push(ty);
                }
            } */
        }
    }

    /// List params and locals as a list of ValType. The func_index
    /// corresponds to a local function, NOT an import function.
    fn get_params_and_locals(&self, func_index: u32) -> Vec<ValType> {
        let mut params_and_locals = Vec::new();
        let absolute_func_index = func_index + self.module_data.num_of_import_functions;
        let func_type_index = self.module_data.function_to_type[absolute_func_index as usize];
        params_and_locals.extend(self.module_data.params[func_type_index as usize].clone());
        for (num, ty) in self.module_data.locals[func_index as usize].clone() {
            for _ in 0..num {
                params_and_locals.push(ty);
            }
        }
        params_and_locals
    }

    /// Given a func index, assign to each variable a memory address where
    /// its checkpoint will be store.
    fn compute_var_to_mem_mapping(&mut self, func_index: u32)
    {
        // Prepare a list of params and locals.
        let params_and_locals = self.get_params_and_locals(func_index);

        // Add resuming_block_var as a local of type I32.
        // This variable is not considered in params_and_locals, because it already
        // has an address associated (namely 0x0), stored in self.resuming_block_addr.
        self.module_data.locals[func_index as usize].push((1, ValType::I32));

        // Save the index of resuming_block_var.
        let mut locals_len = 0;
        for (num, _ty) in &self.module_data.locals[func_index as usize] {
            locals_len += num;
        }
        // self.resuming_block_var = Some(locals_len - 1);
        self.resuming_block_var = Some(params_and_locals.len() as u32);

        // Map each variable to its memory address.
        let mut next_address = self.resuming_block_addr + 1;
        /* let mut all_locals = self.params.clone();
        all_locals.extend(self.locals.clone()); */
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
            self.checkpoint_var_memory_mapping.insert(local_index as u32, next_address);
            next_address += size;
        }

        // Then repeat the same operation for the globals.
        for global_index in 0..self.module_data.global_section.len() {
            let g = self.module_data.global_section[global_index].clone();
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
            self.checkpoint_global_memory_mapping.insert(global_index as u32, next_address);
            next_address += size;
        }
    }

    /// Decide where to place checkpoints.
    fn generate_checkpoint_list(&mut self, _nesting_level: NestingLevel) {
        // In this version, each first-level block will be checkpointed.
        for block in &self.blocks {
            if block.outer == 0 {
                self.checkpoint_list.push(block.index);
            }
        }
    }

    /// Generate a mapping from each checkpoint to the list of variables
    /// modified since the previous checkpoint.
    fn generate_live_out_var_list(&mut self, func_index: u32) {
        // Iterate over each checkpoint.
        for &checkpoint in &self.checkpoint_list {

            // Select the block corresponding this checkpoint.
            let block_index = checkpoint - 1;
            // self.checkpoint_list.iter().position(|&i| i == checkpoint).unwrap();
            let block = &self.blocks[block_index];

            let mut live_out_var: HashSet<u32> = HashSet::new();
            let mut set_global: HashSet<u32> = HashSet::new();

            let mut binary_reader =
                self.module_data.code_section[func_index as usize].get_binary_reader();

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
            for g_index in 0..self.module_data.global_section.len() {
                if self.module_data.global_section[g_index].mutable {
                    set_global.insert(g_index as u32);
                }
            }

            self.live_out_set_vars.push(live_out_var);
            self.set_global_vars.push(set_global);
        }
    }

    /// Load the value of the resuming_block_var, which can be
    /// in one of the following three states:
    /// 1. None, so it has not been initialized yet.
    /// 2. 0, the function is not resuming, the execution should proceed normally.
    /// 3. 1..n, the index of the checkpoint block from which resuming.
    /// This value is stored in the linear memory at a fixed address.
    fn initialize_resuming_block_var(&self, index_restore_memory: u32) -> Vec<Instruction> {
        let mut insts : Vec<Instruction> = Vec::new();
        insts.push(Instruction::Block(BlockType::Empty));
        // Restore the linear memory.
        insts.push(Instruction::Call(index_restore_memory));
        // Push in the stack resuming_block_addr.
        insts.push(Instruction::I32Const(self.resuming_block_addr as i32));
        // Load from memory the value of the resuming block.
        insts.push(Instruction::I32Load(MemArg {
            offset: 0, align: 2, memory_index: self.checkpoint_memory_index,
        }));
        // Initialize the value of resming_block_var.
        insts.push(Instruction::LocalSet(self.resuming_block_var.unwrap()));
        insts.push(Instruction::End);
        insts
    }

    /// Restore the live-out vars of a block (restoring).
    fn resume_live_out_vars(&self, func_index: u32, block_index: usize) -> Vec<Instruction> {

        // Get the index of the block with index block_index in checkpoint_index.
        let checkpoint_index =
            self.checkpoint_list.iter().position(|&i| i == block_index).unwrap();

        // Produce a vector with all the local variables to be checkpointed.
        let mut live_out_vars =
            self.live_out_set_vars[checkpoint_index].iter().cloned().collect::<Vec<u32>>();
        let mut insts = Vec::new();

        // Compute the list of all variables (locals and params).
        let params_and_locals = self.get_params_and_locals(func_index);

        // Produce a list with all the global variables to be checkpointed.
        let mut global_vars =
            self.set_global_vars[checkpoint_index].iter().cloned().collect::<Vec<u32>>();

        // Open a new block.
        insts.push(Instruction::Block(BlockType::Empty));

        // println!("Live out vars {:?}", live_out_vars);
        // println!("params_and_locals {:?}", params_and_locals);

        // Check if we are resuming. So if the resuming_block_var is 0,
        // simply jump to the end of this block.
        insts.push(Instruction::LocalGet(self.resuming_block_var.unwrap()));
        insts.push(Instruction::I32Const(0));
        insts.push(Instruction::I32Eq);
        insts.push(Instruction::BrIf(0));

        // Then insert the load instructions.
        while let Some(var) = live_out_vars.pop() {
            let var_index = usize::try_from(var).unwrap();
            // Push the memory address of the var checkpoint to the stack top.
            insts.push(Instruction::I32Const(*self.checkpoint_var_memory_mapping.get(&var).unwrap() as i32));
            // Load from memory the value of the var.
            let mem_arg = MemArg {
                offset: 0,
                align: 2,
                memory_index: self.checkpoint_memory_index,
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
            insts.push(Instruction::I32Const(*self.checkpoint_global_memory_mapping.get(&global).unwrap() as i32));
            let mem_arg = MemArg {
                offset: 0,
                align: 2,
                memory_index: self.checkpoint_memory_index,
            };
            match self.module_data.globals[global_index] {
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
    fn checkpoint_live_out_vars(&self, func_index: u32, block_index: usize) -> Vec<Instruction> {
        let checkpoint_index =
            self.checkpoint_list.iter().position(|&i| i == block_index).unwrap();
        let mut insts = Vec::new();

        // Open a new block.
        insts.push(Instruction::Block(BlockType::Empty));

        for &var in self.live_out_set_vars[checkpoint_index].iter() {
            let var_index = usize::try_from(var).unwrap();

            // Push the memory address of the var to the stack top.
            insts.push(Instruction::I32Const(*self.checkpoint_var_memory_mapping.get(&var).unwrap() as i32));
            // Push the address in memory.
            insts.push(Instruction::LocalGet(var));

            // Store from memory the value of the var.
            let mem_arg = MemArg {
                offset: 0,
                align: 2,
                memory_index: self.checkpoint_memory_index,
            };

            // Compute the list of all variables (locals and params).
            let params_and_locals = self.get_params_and_locals(func_index);

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

        for &global in self.set_global_vars[checkpoint_index].iter() {
            let global_index = usize::try_from(global).unwrap();

            // Push the memory address of the var to the stack top.
            insts.push(Instruction::I32Const(*self.checkpoint_global_memory_mapping.get(&global).unwrap() as i32));
            // Push the address in memory.
            insts.push(Instruction::GlobalGet(global));

            // Store from memory the value of the var.
            let mem_arg = MemArg {
                offset: 0,
                align: 2,
                memory_index: self.checkpoint_memory_index,
            };

            // Compute the list of all variables (locals and params).
            let params_and_locals = self.get_params_and_locals(func_index);

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
        insts.push(Instruction::I32Const(self.resuming_block_addr as i32));
        insts.push(Instruction::I32Const(block_index as i32));
        insts.push(Instruction::I32Store(MemArg {
            offset: 0,
            align: 2,
            memory_index: self.checkpoint_memory_index,
        }));

        // Then close the block.
        insts.push(Instruction::End);

        insts
    }

    /// Check the resuming_block_var. If 0, the function is not resuming, and
    /// can continue to run, otherwise:
    /// 1. If resuming_block_var == block_index, the function is resuming from this block,
    /// continue executing.
    /// 2. If resuming_block_var /= block_index, jump to the end of this block, the destination
    /// is further down.
    fn jump_to_end(&self, block_index: usize) -> Vec<Instruction> {
        let mut insts = Vec::new();
        // Open a new block.
        insts.push(Instruction::Block(BlockType::Empty));
        // Get the value of resuming_block_var.
        insts.push(Instruction::LocalGet(self.resuming_block_var.unwrap()));
        // Push in the stack the index of the current block.
        insts.push(Instruction::I32Const(block_index as i32));
        // Check if they are not equal.
        insts.push(Instruction::I32Eq);
        // If they are equal, exit this block and resume the execution.
        insts.push(Instruction::BrIf(0));
        // Otherwise, check if we are resuming at all (hence see if
        // self.resuming_block_var is 0, if so, exit this block and
        // continue the execution).
        insts.push(Instruction::LocalGet(self.resuming_block_var.unwrap()));
        insts.push(Instruction::I32Const(0));
        insts.push(Instruction::I32Eq);
        insts.push(Instruction::BrIf(0));
        // Otherwise, jump to the end of the checkpoint region.
        insts.push(Instruction::Br(1));
        // Close this block.
        insts.push(Instruction::End);
        insts
    }

    fn print_block_instructions(&self, block_index: usize, function_body: &FunctionBody<'a>)
        -> Vec<Instruction<'a>>
    {
        let block = &self.blocks[block_index - 1];
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
                            if index >= self.module_data.num_of_import_functions {
                                new_index += 2;
                            }
                            insts.push(Instruction::Call(new_index));
                        }
                        Ok(Instruction::ReturnCall(index)) => {
                            let mut  new_index = index;
                            if index >= self.module_data.num_of_import_functions {
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
    fn update_function_indexes(&mut self, function_body: &FunctionBody, locals: Vec<(u32, ValType)>)
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
                    if index >= self.module_data.num_of_import_functions {
                        new_index += 2;
                    }
                    new_function_body.instruction(&Instruction::Call(new_index));
                }
                Ok(Instruction::ReturnCall(index)) => {
                    let mut  new_index = index;
                    if index >= self.module_data.num_of_import_functions {
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

    /// Insert the checkpoint and restore instructions in the correct position.
    fn produce_final_bytecode(&mut self, func_index: u32) -> Vec<u8>
    {
        assert!(self.module_data.code_section.len() >= 1);
        let func_index = usize::try_from(func_index).unwrap();

        // The module with the additional runtime procedures for checkpoint and restore.
        let mut module = Module::new();

        // ------------------------------- //
        //    Encode the type section.     //
        // ------------------------------- //
        let mut types = TypeSection::new();
        if let Some(type_section) = self.module_data.type_section.clone() {
            types = type_section;
        } else {
            let func_type_index = self.module_data.function_to_type[func_index];
            let params = self.module_data.params[func_type_index as usize].clone();
            let results = self.module_data.results[func_type_index as usize].clone();
            types.ty().function(params, results);
        }

        // Add the type of host-imported should_migrate.
        let index_should_migrate = types.len();
        let index_restore_memory = index_should_migrate + 1;
        types.ty().function(Vec::default(), [ValType::I32]);
        types.ty().function(Vec::default(), []);
        module.section(&types);

        // ------------------------------- //
        //    Encode the import section.   //
        // ------------------------------- //
        let mut imports = ImportSection::new();
        if let Some(im) = self.module_data.import_section.clone() {
            imports = im;
        }
        imports.import("host", "should_migrate",
                       EntityType::Function(index_should_migrate));
        imports.import("host", "restore_memory",
                       EntityType::Function(index_restore_memory));
        module.section(&imports);

        // --------------------------------- //
        //    Encode the function section.   //
        // --------------------------------- //
        let mut functions = wasm_encoder::FunctionSection::new();
        if let Some(function_section) = self.module_data.function_section.clone() {
            functions = function_section;
        } else {
            // let type_index = types.len() - 1;
            let type_index = 0;
            functions.function(type_index);
        }
        module.section(&functions);

        // Build the set of locals in the format accepted by Function::new().
        /* let mut locals = Vec::new();
        for &l in self.locals.iter() {
            if locals.is_empty() {
                locals.push((1, l));
            } else {
                let last = locals.last().unwrap();
                let mut new_last = (1, l);
                if l == last.1 {
                    new_last.0 += last.0;
                    locals.pop();
                }
                locals.push(new_last);
            }
        } */

        // ------------------------------ //
        //    Encode the table section.   //
        // ------------------------------ //
        let mut tables = wasm_encoder::TableSection::new();
        if let Some(table_section) = self.module_data.table_section.clone() {
            tables = table_section;
        }
        module.section(&tables);

        // ------------------------------- //
        //    Encode the memory section.   //
        // ------------------------------- //
        // Encode the linear memory.
        let mut memories = MemorySection::new();
        if let Some(memory_section) = self.module_data.memory_section.clone() {
            memories = memory_section;
        }
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

        // ------------------------------- //
        //    Encode the global section.   //
        // ------------------------------- //
        /* if let Some(global_section) = self.module_data.global_section.clone() {
            module.section(&global_section);
        } */
        let mut globals = wasm_encoder::GlobalSection::new();
        for i in 0..self.module_data.global_section.len() {
            globals.global(self.module_data.global_section[i],
                           &self.module_data.global_init_expr[i]);
        }
        module.section(&globals);

        // ------------------------------- //
        //    Encode the export section.   //
        // ------------------------------- //
        let mut exports = wasm_encoder::ExportSection::new();
        /* if let Some(export_section) = self.module_data.export_section.clone() {
            module.section(&export_section);
        } */
        for export in self.module_data.exports.clone() {
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
        exports.export("checkpoint_memory", ExportKind::Memory, self.checkpoint_memory_index);
        exports.export("checkpoint_l_memory", ExportKind::Memory, self.checkpoint_memory_index + 1);
        module.section(&exports);

        // ------------------------------- //
        //    Encode the start section.    //
        // ------------------------------- //
        /* module.section(&StartSection {
            function_index: 11,
        }); */

        // -------------------------------- //
        //    Encode the element section.   //
        // -------------------------------- //
        let mut elements = wasm_encoder::ElementSection::new();
        if let Some(element_section) = self.module_data.element_section.clone() {
            elements = element_section;
        }
        module.section(&elements);

        // ----------------------------- //
        //    Encode the code section.   //
        // ----------------------------- //
        let mut codes = CodeSection::new();

        // First, encode the function before func_index.
        for i in 0..func_index {
            let func_body = self.module_data.code_section[i].clone();
            let locals = self.module_data.locals[i].clone();
            let func = self.update_function_indexes(&func_body, locals);
            codes.function(&func);
        }

        // Then encode the function at func_index.
        let locals = self.module_data.locals[func_index].clone();
        let mut f = Function::new(locals);

        // Add the initial resuming instructions.
        let index_of_restore_memory = self.module_data.num_of_import_functions + 1;
        for inst in self.initialize_resuming_block_var(index_of_restore_memory) {
            f.instruction(&inst);
        }

        // Add the checkpoint and restore instructions.
        let function_body =
            self.module_data.code_section[func_index].clone();
        let index_of_should_migrate = self.module_data.num_of_import_functions;
        let mut previous_block_index = None;
        for block in self.blocks.iter() {
            if self.checkpoint_list.contains(&block.index) {

                // First, check whether this is the last region or the first. If so, there are
                // a few things to mend.
                let is_last = &block.index == self.checkpoint_list.last().unwrap();
                let is_first = &block.index == self.checkpoint_list.first().unwrap();

                // Place into a block.
                if !is_last {
                    f.instruction(&Instruction::Block(BlockType::Empty));
                }

                // Add the resume instructions for this block predecessor.
                if !is_first {
                    for inst in self.resume_live_out_vars(func_index as u32, previous_block_index.unwrap()) {
                        f.instruction(&inst);
                    }
                }
                previous_block_index = Some(block.index);

                // Add the jump instructions for this block.
                if !is_last {
                    for inst in self.jump_to_end(block.index) {
                        f.instruction(&inst);
                    }
                }

                // Load all the instructions in the inner blocks.
                for inst in self.print_block_instructions(block.index, &function_body) {
                    f.instruction(&inst);
                }

                // At the end of the block, add the checkpoint insts only if this is
                // not the last region in the function.
                if !is_last {
                    for inst in self.checkpoint_live_out_vars(func_index as u32, block.index) {
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
        codes.function(&f);

        // Finally, encode the functions after func_index.
        for i in (func_index + 1)..self.module_data.code_section.len() {
            let func_body = self.module_data.code_section[i].clone();
            let locals = self.module_data.locals[i].clone();
            let func = self.update_function_indexes(&func_body, locals);
            codes.function(&func);
        }

        module.section(&codes);

        /* let mut exports = ExportSection::new();
        exports.export("gcd", ExportKind::Func, 0);
        module.section(&exports); */

        // ----------------------------- //
        //    Encode the data section.   //
        // ----------------------------- //
        // Encode the previous data section.
        if let Some(data_section) = self.module_data.data_section.clone() {
            module.section(&data_section);
        }

        // Extract the encoded Wasm bytes for this module.
        let wasm_bytes = module.finish();

        wasm_bytes
    }

    /// Insert the instructions to perform checkpoint and restore.
    pub fn compute(&mut self, wasm_bytes: &'a Vec<u8>, func_index: u32) -> Vec<u8> {

        // Parse the module bytecode.
        let parser = Parser::new(0);
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
                _ => {}
            }
        }

        // Extract locals, params and results for all functions.
        self.extract_locals();

        // Build a tree out of the Wasm bytecode, first step to define regions.
        self.build_block_tree(func_index);

        // Divide the function into regions.
        self.generate_checkpoint_list(NestingLevel::ONE);

        // Compute the list of live-out variables to checkpoint.
        self.generate_live_out_var_list(func_index);

        // Assign a memory address to all the variables to be checkpointed.
        self.compute_var_to_mem_mapping(func_index);

        self.produce_final_bytecode(func_index)
    }

    /// Print relevan information for evaluation purposes.
    pub fn print_module_info(&self, func_index: u32, name: String) -> ComputationInfo {

        let comp_info = ComputationInfo {
            name,
            blocks: {
                let mut blocks = vec![];
                for b in &self.blocks {
                    blocks.push(b.index as u32);
                }
                blocks
            },
            params_and_locals: {
                let mut params_and_locals = vec![];
                for l in &self.get_params_and_locals(func_index) {
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
                for c in &self.checkpoint_list {
                    checkpoints.push(*c as u32);
                }
                checkpoints
            },
            live_out_vars: {
                let mut live_out_vars = vec![];
                for l in &self.live_out_set_vars {
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
    use std::fs::File;
    use std::io::{Read, Write};
    use crate::WasmMigrate;

    #[test]
    fn add_checkpoint_at() -> Result<(), Box<dyn std::error::Error>> {
        let mut migration_injector: WasmMigrate = WasmMigrate::new();

        // Load the module Wasm bytecode.
        let mut file = File::open("./tests/gcd.wasm").unwrap();
        let mut wasm_bytes = Vec::new();
        let _ = file.read_to_end(&mut wasm_bytes);
        let func_index = 12;

        let modified_body =
            migration_injector.compute(&wasm_bytes.clone(), func_index);

        // For demonstration, let's just write the modified body to a new file
        let mut output_file =
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open("./tests/gcd_with_checkpoints.wasm")?;

        let _ = output_file.write(modified_body.as_slice());

        Ok(())
    }
}