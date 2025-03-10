use super::*;
use crate::{cond_stmt::CondStmt, executor::StatusType};
use crate::dyncfg::cfg::ControlFlowGraph;
use rand;
use std::{
    fs,
    io::prelude::*,
    mem,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex, RwLock, Arc
    },
};
// https://crates.io/crates/priority-queue
use angora_common::config;
use priority_queue::PriorityQueue;

pub struct Depot {
    pub queue: Mutex<PriorityQueue<CondStmt, QPriority>>,
    pub num_inputs: AtomicUsize,
    pub num_hangs: AtomicUsize,
    pub num_crashes: AtomicUsize,
    pub dirs: DepotDir,
    pub cfg: RwLock<ControlFlowGraph>,
}

impl Depot {
    pub fn new(in_dir: PathBuf, out_dir: &Path, cfg: RwLock<ControlFlowGraph>) -> Self {
        Self {
            queue: Mutex::new(PriorityQueue::new()),
            num_inputs: AtomicUsize::new(0),
            num_hangs: AtomicUsize::new(0),
            num_crashes: AtomicUsize::new(0),
            dirs: DepotDir::new(in_dir, out_dir),
            cfg
        }
    }

    fn save_input(
        status: &StatusType,
        buf: &Vec<u8>,
        num: &AtomicUsize,
        cmpid: u32,
        dir: &Path,
    ) -> usize {
        let id = num.fetch_add(1, Ordering::Relaxed);
        trace!(
            "Find {} th new {:?} input by fuzzing {}.",
            id,
            status,
            cmpid
        );
        let new_path = get_file_name(dir, id);
        let mut f = fs::File::create(new_path.as_path()).expect("Could not save new input file.");
        f.write_all(buf)
            .expect("Could not write seed buffer to file.");
        f.flush().expect("Could not flush file I/O.");
        id
    }

    pub fn save(&self, status: StatusType, buf: &Vec<u8>, cmpid: u32) -> usize {
        match status {
            StatusType::Normal => {
                Self::save_input(&status, buf, &self.num_inputs, cmpid, &self.dirs.inputs_dir)
            },
            StatusType::Timeout => {
                Self::save_input(&status, buf, &self.num_hangs, cmpid, &self.dirs.hangs_dir)
            },
            StatusType::Crash => Self::save_input(
                &status,
                buf,
                &self.num_crashes,
                cmpid,
                &self.dirs.crashes_dir,
            ),
            _ => 0,
        }
    }

    pub fn empty(&self) -> bool {
        self.num_inputs.load(Ordering::Relaxed) == 0
    }

    pub fn next_random(&self) -> usize {
        rand::random::<usize>() % self.num_inputs.load(Ordering::Relaxed)
    }

    pub fn get_input_buf(&self, id: usize) -> Vec<u8> {
        let path = get_file_name(&self.dirs.inputs_dir, id);
        read_from_file(&path)
    }

    pub fn get_entry(&self) -> Option<(CondStmt, QPriority)> {
        let mut q = match self.queue.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                warn!("Mutex poisoned! Results may be incorrect. Continuing...");
                poisoned.into_inner()
            },
        };
        q.peek()
            .and_then(|x| Some((x.0.clone(), x.1.clone())))
            .and_then(|x| {
                if !x.1.is_done() {
                    let q_inc = x.1.inc(x.0.base.op);
                    q.change_priority(&(x.0), q_inc);
                }
                Some(x)
            })
    }

    pub fn add_entries(&self, conds: Vec<CondStmt>,  target_cond : (u32,u32), branch_cov : &Arc<Mutex<Vec<(u32,u32,u32,u32)>>>) {
        let mut q = match self.queue.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                warn!("Mutex poisoned! Results may be incorrect. Continuing...");
                poisoned.into_inner()
            },
        };

        for mut cond in conds {
            if cond.is_desirable {

                let cfg = self.cfg.read().unwrap();
                //let distance = cfg.score_for_cmp(cond.base.cmpid);
                let distance = cfg.score_for_cmp_inp(cond.base.cmpid, cond.variables.clone());
                drop(cfg); // No need to hold the lock
                if let Some(v) = q.get_mut(&cond) {
                    if !v.0.is_done() {
                        // If existed one and our new one has two different conditions,
                        // this indicate that it is explored.
                        if v.0.base.condition != cond.base.condition {
                            let mut branch_cov_write = match branch_cov.lock() {
                                Ok(g) => g,
                                Err(p) => p.into_inner(),
                            };
                            (*branch_cov_write).push((target_cond.0,target_cond.1,v.0.base.cmpid, v.0.base.func));
                            v.0.mark_as_done();
                            q.change_priority(&cond, QPriority::done());
                        } else {
                            // Existed, but the new one are better
                            // If the cond is faster than the older one, we prefer the faster,
                            if config::PREFER_FAST_COND && v.0.speed > cond.speed {
                                mem::swap(v.0, &mut cond);
                                let priority = QPriority::init_distance(cond.base.op, distance);
                                q.change_priority(&cond, priority);
                            }
                        }
                    }
                } else {
                    let priority = QPriority::init_distance(cond.base.op, distance);
                    q.push(cond, priority);
                }
            }
        }
    }

    pub fn update_entry(&self, cond: CondStmt) {
        let mut q = match self.queue.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                warn!("Mutex poisoned! Results may be incorrect. Continuing...");
                poisoned.into_inner()
            },
        };
        if let Some(v) = q.get_mut(&cond) {
            v.0.clone_from(&cond);
            let cfg = self.cfg.read().unwrap();
            let distance = cfg.score_for_cmp(cond.base.cmpid);
            let p = v.1.new_distance(distance);
            q.change_priority(&cond, p);
        } else {
            warn!("Update entry: can not find this cond");
        }
        if cond.is_discarded() {
            q.change_priority(&cond, QPriority::done());
            let mut cfg = self.cfg.write().unwrap();
            cfg.remove_target(cond.base.cmpid);
        }
    }
}
