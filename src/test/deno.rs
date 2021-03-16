use deno_core::serde_json::{json, Value};
use deno_core::json_op_sync;
use deno_core::error::AnyError;
use deno_runtime::permissions::Permissions;
use deno_runtime::worker::MainWorker;
use deno_runtime::worker::WorkerOptions;
use std::rc::Rc;
use std::sync::Arc;
use serde::Serialize;
use serde::de::DeserializeOwned;
use deno_core::{OpFn};
use super::source_maps::apply_source_map;
use super::file_fetcher::File;
use super::media_type::MediaType;
use super::flags::Flags;
use super::program_state::ProgramState;
use super::tools;
use super::ops;
use super::fmt_errors::PrettyJsError;
use super::module_loader::CliModuleLoader;
use deno_core::ModuleSpecifier;
use deno_runtime::web_worker::WebWorkerOptions;
use deno_runtime::ops::worker_host::CreateWebWorkerCb;
use deno_runtime::web_worker::WebWorker;

mod sessions {
    use std::sync::Mutex;
    use std::fs;
    use std::env;
    use std::collections::HashMap;
    use deno_core::error::AnyError;
    use clarity_repl::repl::{self, Session};
    use clarity_repl::repl::settings::Account;
    use crate::types::{ChainConfig, MainConfig};

    lazy_static! {
        static ref SESSIONS: Mutex<HashMap<u32, Session>> = Mutex::new(HashMap::new());
    }

    pub fn handle_setup_chain() -> Result<(u32, Vec<Account>), AnyError> {
        let mut sessions = SESSIONS.lock().unwrap();
        let session_id = sessions.len() as u32;

        let mut settings = repl::SessionSettings::default();
        let root_path = env::current_dir().unwrap();
        let mut project_config_path = root_path.clone();
        project_config_path.push("Clarinet.toml");
    
        let mut chain_config_path = root_path.clone();
        chain_config_path.push("settings");
        chain_config_path.push("Local.toml");
    
        let project_config = MainConfig::from_path(&project_config_path);
        let chain_config = ChainConfig::from_path(&chain_config_path);
    
        for (name, config) in project_config.contracts.iter() {
            let mut contract_path = root_path.clone();
            contract_path.push(&config.path);
    
            let code = fs::read_to_string(&contract_path).unwrap();
    
            settings
                .initial_contracts
                .push(repl::settings::InitialContract {
                    code: code,
                    name: Some(name.clone()),
                    deployer: Some("ST1D0XTBR7WVNSYBJ7M26XSJAXMDJGJQKNEXAM6JH".to_string()),
                });
        }
    
        for (name, account) in chain_config.accounts.iter() {
            settings
                .initial_accounts
                .push(repl::settings::Account {
                    name: name.clone(),
                    balance: account.balance,
                    address: account.address.clone(),
                    mnemonic: account.mnemonic.clone(),
                    derivation: account.derivation.clone(),
                });
        }

        let session = Session::new(settings.clone());
        sessions.insert(session_id, session);
        Ok((session_id, settings.initial_accounts))
    }

    pub fn get_session() -> Result<(), AnyError> {
        Ok(())
    }
}

pub async fn run_tests() -> Result<(), AnyError> {

    let fail_fast = true;
    let quiet = false;
    let filter = None;

    let mut flags = Flags::default();
    flags.unstable = true;
    let program_state = ProgramState::build(flags.clone()).await?;
    let permissions = Permissions::from_options(&flags.clone().into());
    let cwd = std::env::current_dir().expect("No current directory");
    let include = vec![".".to_string()];
    let test_modules =
      tools::test_runner::prepare_test_modules_urls(include, &cwd)?;
  
    if test_modules.is_empty() {
      println!("No matching test modules found");
      return Ok(());
    }
    let main_module = deno_core::resolve_path("$deno$test.ts")?;
    // Create a dummy source file.

    let source = tools::test_runner::render_test_file(
      test_modules.clone(),
      fail_fast,
      quiet,
      filter,
    );

    let source_file = File {
      local: main_module.to_file_path().unwrap(),
      maybe_types: None,
      media_type: MediaType::TypeScript,
      source,
      specifier: main_module.clone(),
    };

    // Save our fake file into file fetcher cache
    // to allow module access by TS compiler
    program_state.file_fetcher.insert_cached(source_file);
  
    let mut worker =
      create_main_worker(&program_state, main_module.clone(), permissions);

    worker.js_runtime.register_op("setup_chain", op(setup_chain));

    worker.execute_module(&main_module).await?;
    worker.execute("window.dispatchEvent(new Event('load'))")?;
    worker.run_event_loop().await?;
    worker.execute("window.dispatchEvent(new Event('unload'))")?;
    worker.run_event_loop().await?;

    Ok(())
}

fn create_web_worker_callback(
    program_state: Arc<ProgramState>,
  ) -> Arc<CreateWebWorkerCb> {
    Arc::new(move |args| {
      let global_state_ = program_state.clone();
      let js_error_create_fn = Rc::new(move |core_js_error| {
        let source_mapped_error =
          apply_source_map(&core_js_error, global_state_.clone());
        PrettyJsError::create(source_mapped_error)
      });
  
      let attach_inspector = program_state.maybe_inspector_server.is_some()
        || program_state.coverage_dir.is_some();
      let maybe_inspector_server = program_state.maybe_inspector_server.clone();
  
      let module_loader = CliModuleLoader::new_for_worker(
        program_state.clone(),
        args.parent_permissions.clone(),
      );
      let create_web_worker_cb =
        create_web_worker_callback(program_state.clone());
  
      let options = WebWorkerOptions {
        args: program_state.flags.argv.clone(),
        apply_source_maps: true,
        debug_flag: false,
        unstable: program_state.flags.unstable,
        ca_data: program_state.ca_data.clone(),
        user_agent: super::version::get_user_agent(),
        seed: program_state.flags.seed,
        module_loader,
        create_web_worker_cb,
        js_error_create_fn: Some(js_error_create_fn),
        use_deno_namespace: args.use_deno_namespace,
        attach_inspector,
        maybe_inspector_server,
        runtime_version: super::version::deno(),
        ts_version: super::version::TYPESCRIPT.to_string(),
        no_color: !super::colors::use_color(),
        get_error_class_fn: None,
      };
  
      let mut worker = WebWorker::from_options(
        args.name,
        args.permissions,
        args.main_module,
        args.worker_id,
        &options,
      );
  
      // This block registers additional ops and state that
      // are only available in the CLI
      {
        let js_runtime = &mut worker.js_runtime;
        js_runtime
          .op_state()
          .borrow_mut()
          .put::<Arc<ProgramState>>(program_state.clone());
        // Applies source maps - works in conjuction with `js_error_create_fn`
        // above
        ops::errors::init(js_runtime);
        if args.use_deno_namespace {
          ops::runtime_compiler::init(js_runtime);
        }
      }
      worker.bootstrap(&options);
  
      worker
    })
  }

pub fn create_main_worker(
    program_state: &Arc<ProgramState>,
    main_module: ModuleSpecifier,
    permissions: Permissions,
  ) -> MainWorker {
    let module_loader = CliModuleLoader::new(program_state.clone());
  
    let global_state_ = program_state.clone();
  
    let js_error_create_fn = Rc::new(move |core_js_error| {
      let source_mapped_error =
        apply_source_map(&core_js_error, global_state_.clone());
      PrettyJsError::create(source_mapped_error)
    });
  
    let attach_inspector = program_state.maybe_inspector_server.is_some()
      || program_state.flags.repl
      || program_state.coverage_dir.is_some();
    let maybe_inspector_server = program_state.maybe_inspector_server.clone();
    let should_break_on_first_statement =
      program_state.flags.inspect_brk.is_some();
  
    let create_web_worker_cb = create_web_worker_callback(program_state.clone());
  
    let options = WorkerOptions {
      apply_source_maps: true,
      args: program_state.flags.argv.clone(),
      debug_flag: false,
      unstable: program_state.flags.unstable,
      ca_data: program_state.ca_data.clone(),
      user_agent: super::version::get_user_agent(),
      seed: program_state.flags.seed,
      js_error_create_fn: Some(js_error_create_fn),
      create_web_worker_cb,
      attach_inspector,
      maybe_inspector_server,
      should_break_on_first_statement,
      module_loader,
      runtime_version: super::version::deno(),
      ts_version: super::version::TYPESCRIPT.to_string(),
      no_color: !super::colors::use_color(),
      get_error_class_fn: None,
      location: program_state.flags.location.clone(),
    };
  
    let mut worker = MainWorker::from_options(main_module, permissions, &options);
  
    // This block registers additional ops and state that
    // are only available in the CLI
    {
      let js_runtime = &mut worker.js_runtime;
      js_runtime
        .op_state()
        .borrow_mut()
        .put::<Arc<ProgramState>>(program_state.clone());
      // Applies source maps - works in conjuction with `js_error_create_fn`
      // above
      ops::errors::init(js_runtime);
      ops::runtime_compiler::init(js_runtime);
    }
    worker.bootstrap(&options);
  
    worker
  }

  
fn get_error_class_name(e: &AnyError) -> &'static str {
    deno_runtime::errors::get_error_class_name(e).unwrap_or("Error")
}

fn op<F, V, R>(op_fn: F) -> Box<OpFn>
where
  F: Fn(V) -> Result<R, AnyError> + 'static,
  V: DeserializeOwned,
  R: Serialize,
{
    json_op_sync(move |s, args, _bufs| {
        op_fn(args)
    })    
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetupChainArgs {
//   specifier: String,
//   version: String,
//   start: usize,
//   end: usize,
}

fn setup_chain(args: SetupChainArgs) -> Result<Value, AnyError> {
    let (session_id, accounts) = sessions::handle_setup_chain()?;

    Ok(json!({
        "session_id": session_id,
        "accounts": accounts,
    }))
}

fn mine_block() {

}

fn set_tx_sender() {

}

fn get_accounts() {
    
}