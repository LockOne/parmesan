use crate::stats::*;
use crate::dyncfg::{cfg::ControlFlowGraph, fparse::parse_targets_file};
use angora_common::defs;
use chrono::prelude::Local;
use std::{
    collections::HashMap,
    fs,
    io::prelude::*,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, RwLock, Mutex
    },
    io,
    thread, time,
    ops::Deref,
};

use crate::{bind_cpu, branches, check_dep, command, depot, executor, fuzz_loop, stats};
use ctrlc;
use libc;
use pretty_env_logger;

pub fn fuzz_main(
    mode: &str,
    in_dir: &str,
    out_dir: &str,
    track_target: &str,
    pargs: Vec<String>,
    num_jobs: usize,
    mem_limit: u64,
    time_limit: u64,
    search_method: &str,
    sync_afl: bool,
    enable_afl: bool,
    enable_exploitation: bool,
    cfg_input_file: &str,
    sanopt_target: Option<&str>,
    directed_only: bool,
    num_of_func: Option<&str>
) {
    pretty_env_logger::init();

    debug!("logger test");

    let (seeds_dir, angora_out_dir) = initialize_directories(in_dir, out_dir, sync_afl);
    let parmesan_info = parse_targets_file(Path::new(&cfg_input_file)).expect("Could not read cfg targets file");
    let cfg = ControlFlowGraph::new(parmesan_info);

    let command_option = command::CommandOpt::new(
        mode,
        track_target,
        pargs,
        &angora_out_dir,
        search_method,
        mem_limit,
        time_limit,
        enable_afl,
        enable_exploitation,
        cfg_input_file,
        sanopt_target,
        directed_only,
    );
    info!("{:?}", command_option);

    check_dep::check_dep(in_dir, out_dir, &command_option);

    let depot = Arc::new(depot::Depot::new(seeds_dir, &angora_out_dir, RwLock::new(cfg.clone())));
    info!("{:?}", depot.dirs);

    let stats = Arc::new(RwLock::new(stats::ChartStats::new()));
    let global_branches = Arc::new(branches::GlobalBranches::new(RwLock::new(cfg)));
    let fuzzer_stats = create_stats_file_and_write_pid(&angora_out_dir);
    let running = Arc::new(AtomicBool::new(true));
    set_sigint_handler(running.clone());
    let func_num = get_func_num(num_of_func); 

    let func_rel_map = vec![vec![0usize; func_num].into_boxed_slice(); func_num].into_boxed_slice();
    let func_rel_map = Arc::new(RwLock::new(func_rel_map));

    //target_id,target_func,coverd_id, covered_func
    let branch_cov : Arc<Mutex<Vec<(u32,u32,u32,u32)>>> = Arc::new(Mutex::new(vec![])); 

    let mut executor = executor::Executor::new(
        command_option.specify(0),
        global_branches.clone(),
        depot.clone(),
        stats.clone(),
        func_rel_map.clone(),
        branch_cov.clone(),
    );

    depot::sync_depot(&mut executor, running.clone(), &depot.dirs.seeds_dir);

    if depot.empty() {
        error!("Failed to find any branches during dry run.");
        error!("Please ensure that the binary has been instrumented and/or input directory is populated.");
        error!(
            "Please ensure that seed directory - {:?} has any file.",
            depot.dirs.seeds_dir
        );
        panic!();
    }

    // Enable ParmeSan directed-only mode
    executor.set_directed(directed_only);

    let (handles, child_count) = init_cpus_and_run_fuzzing_threads(
        num_jobs,
        &running,
        &command_option,
        &global_branches,
        &depot,
        &stats,
        &func_rel_map,
        &branch_cov,
    );

    let log_file = match fs::File::create(angora_out_dir.join(defs::ANGORA_LOG_FILE)) {
        Ok(a) => a,
        Err(e) => {
            error!("FATAL: Could not create log file: {:?}", e);
            panic!();
        }
    };

    main_thread_sync_and_log(
        log_file,
        out_dir,
        sync_afl,
        running.clone(),
        &mut executor,
        &depot,
        &global_branches,
        &stats,
        child_count,
    );

    for handle in handles {
        if handle.join().is_err() {
            error!("Error happened in fuzzing thread!");
        }
    }

    match fs::remove_file(&fuzzer_stats) {
        Ok(_) => (),
        Err(e) => warn!("Could not remove fuzzer stats file: {:?}", e),
    };

    let read_lock = match func_rel_map.read() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };

    let mut rels_file = match fs::File::create(angora_out_dir.join("func_rels.csv")) {
        Ok(a) => a,
        Err(e) => {
            error!("FATAL: Could not create log file: {:?}", e);
            panic!();
        }
    };

    write!(rels_file,",").unwrap();
    for i in 0..func_num {
        write!(rels_file, "{},", i).unwrap();
    }
    writeln!(rels_file, "").unwrap();

    for i1 in 0..func_num {
        write!(rels_file,"{},",i1).unwrap();
        for i2 in 0..func_num {
            write!(rels_file,"{},",(*read_lock).deref()[i1].deref()[i2]).unwrap();
        }
        writeln!(rels_file,"").unwrap();
    }

    let branch_cov_lock = match branch_cov.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };

    let mut branch_cov_file = match fs::File::create(angora_out_dir.join("branch_cov.txt")) {
        Ok(a) => a,
        Err(e) => {
            error!("FATAL: Could not create log file: {:?}", e);
            panic!();
        }
    };
    writeln!(branch_cov_file,"target cmpid, func, covered cmpid, func").unwrap();
    for (t1,t2,c1,c2) in &*branch_cov_lock {
        writeln!(branch_cov_file, "{},{},{},{}", *t1,*t2,*c1,*c2).unwrap();
    }
}

fn initialize_directories(in_dir: &str, out_dir: &str, sync_afl: bool) -> (PathBuf, PathBuf) {
    let angora_out_dir = if sync_afl {
        gen_path_afl(out_dir)
    } else {
        PathBuf::from(out_dir)
    };

    match fs::create_dir(&angora_out_dir) {
        Ok(_) => {},
        Err(_e) => {
            info!("Output dir already exists!");
            match fs::File::open(angora_out_dir.join("angora.csv")) {
                Ok(f) => {
                    let lines = io::BufReader::new(f).lines();
                    if lines.count() <= 200 {
                        info!("remove previous directory and continue");
                        fs::remove_dir_all(&angora_out_dir).unwrap();
                        fs::create_dir(&angora_out_dir).unwrap();
                    } else {
                        panic!("The outdir already exists, contains too much data, halt.");
                    }
                },
                Err(_) => {
                    match fs::File::open(angora_out_dir.join("cond_queue.csv")) { 
                        Ok(_f) => {  
                            info!("no angora.csv file, but cond_queue.csv file exists, assume it is angora output dir, proceed.");
                            fs::remove_dir_all(&angora_out_dir).unwrap();
                            fs::create_dir(&angora_out_dir).unwrap();
                        },
                        Err(_) => {
                            panic!("Output dir exists but no angora.csv, cond_queue.csv file!");
                        }
                    }
                }
            }
        }
    }

    let seeds_dir = PathBuf::from(in_dir);

    (seeds_dir, angora_out_dir)
}

fn gen_path_afl(out_dir: &str) -> PathBuf {
    let base_path = PathBuf::from(out_dir);
    let create_dir_result = fs::create_dir(&base_path);
    if create_dir_result.is_err() {
        warn!("dir has existed. {:?}", base_path);
    }
    base_path.join(defs::ANGORA_DIR_NAME)
}

fn get_func_num(s : Option<&str>) -> usize {
    if s.is_none() {return 0}
    let func_num = fs::read_to_string(s.unwrap()).expect("Can not read func info file");
    func_num.parse::<usize>().unwrap_or(0)
}

fn set_sigint_handler(r: Arc<AtomicBool>) {
    ctrlc::set_handler(move || {
        warn!("Ending Fuzzing.");
        r.store(false, Ordering::SeqCst);
    })
    .expect("Error setting SIGINT handler!");
}

fn create_stats_file_and_write_pid(angora_out_dir: &PathBuf) -> PathBuf {
    // To be compatible with AFL.
    let fuzzer_stats = angora_out_dir.join("fuzzer_stats");
    let pid = unsafe { libc::getpid() as usize };
    let mut buffer = match fs::File::create(&fuzzer_stats) {
        Ok(a) => a,
        Err(e) => {
            error!("Could not create stats file: {:?}", e);
            panic!();
        }
    };
    write!(buffer, "fuzzer_pid : {}", pid).expect("Could not write to stats file");
    fuzzer_stats
}

fn init_cpus_and_run_fuzzing_threads(
    num_jobs: usize,
    running: &Arc<AtomicBool>,
    command_option: &command::CommandOpt,
    global_branches: &Arc<branches::GlobalBranches>,
    depot: &Arc<depot::Depot>,
    stats: &Arc<RwLock<stats::ChartStats>>,
    func_rel_map : &Arc<RwLock<Box<[Box<[usize]>]>>>,
    branch_cov : &Arc<Mutex<Vec<(u32,u32,u32,u32)>>>,
) -> (Vec<thread::JoinHandle<()>>, Arc<AtomicUsize>) {
    let child_count = Arc::new(AtomicUsize::new(0));
    let mut handlers = vec![];
    let free_cpus = bind_cpu::find_free_cpus(num_jobs);
    let free_cpus_len = free_cpus.len();
    let bind_cpus = if free_cpus_len < num_jobs {
        warn!("The number of free cpus is less than the number of jobs. Will not bind any thread to any cpu.");
        false
    } else {
        true
    };
    for thread_id in 0..num_jobs {
        let c = child_count.clone();
        let r = running.clone();
        let cmd = command_option.specify(thread_id + 1);
        let d = depot.clone();
        let b = global_branches.clone();
        let s = stats.clone();
        let cid = if bind_cpus { free_cpus[thread_id] } else { 0 };
        let f = func_rel_map.clone();
        let b2 = branch_cov.clone();
        let handler = thread::spawn(move || {
            c.fetch_add(1, Ordering::SeqCst);
            if bind_cpus {
                bind_cpu::bind_thread_to_cpu_core(cid);
            }
            fuzz_loop::fuzz_loop(r, cmd, d, b, s, f, b2);
        });
        handlers.push(handler);
    }
    (handlers, child_count)
}

fn main_thread_sync_and_log(
    mut log_file: fs::File,
    out_dir: &str,
    sync_afl: bool,
    running: Arc<AtomicBool>,
    executor: &mut executor::Executor,
    depot: &Arc<depot::Depot>,
    global_branches: &Arc<branches::GlobalBranches>,
    stats: &Arc<RwLock<stats::ChartStats>>,
    child_count: Arc<AtomicUsize>,
) {
    let mut last_explore_num = stats.read().unwrap().get_explore_num();
    let sync_dir = Path::new(out_dir);
    let mut synced_ids = HashMap::new();
    if sync_afl {
        depot::sync_afl(executor, running.clone(), sync_dir, &mut synced_ids);
    }
    let mut sync_counter = 1;
    show_stats(&mut log_file, depot, global_branches, stats);
    while running.load(Ordering::SeqCst) {
        thread::sleep(time::Duration::from_secs(5));
        sync_counter -= 1;
        if sync_afl && sync_counter <= 0 {
            depot::sync_afl(executor, running.clone(), sync_dir, &mut synced_ids);
            sync_counter = 12;
        }

        show_stats(&mut log_file, depot, global_branches, stats);
        if Arc::strong_count(&child_count) == 1 {
            let s = stats.read().unwrap();
            let cur_explore_num = s.get_explore_num();
            if cur_explore_num == 0 {
                warn!("There is none constraint in the seeds, please ensure the inputs are vaild in the seed directory, or the program is ran correctly, or the read functions have been marked as source.");
                break;
            } else {
                if cur_explore_num == last_explore_num {
                    info!("Solve all constraints!!");
                    break;
                }
                last_explore_num = cur_explore_num;
            }
        }
    }
}
