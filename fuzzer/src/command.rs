use crate::{check_dep, search, tmpfs};
use angora_common::defs;
use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

static TMP_DIR: &str = "tmp";
static INPUT_FILE: &str = "cur_input";
static FORKSRV_SOCKET_FILE: &str = "forksrv_socket";
static TRACK_FILE: &str = "track";
static PIN_ROOT_VAR: &str = "PIN_ROOT";

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InstrumentationMode {
    LLVM,
    Pin,
}

impl InstrumentationMode {
    pub fn from(mode: &str) -> Self {
        match mode {
            "llvm" => InstrumentationMode::LLVM,
            "pin" => InstrumentationMode::Pin,
            _ => unreachable!(),
        }
    }

    pub fn is_pin_mode(&self) -> bool {
        self == &InstrumentationMode::Pin
    }
}

#[derive(Debug, Clone)]
pub struct CommandOpt {
    pub mode: InstrumentationMode,
    pub id: usize,
    pub main: (String, Vec<String>),
    pub track: (String, Vec<String>),
    pub tmp_dir: PathBuf,
    pub out_file: String,
    pub forksrv_socket_path: String,
    pub track_path: String,
    pub is_stdin: bool,
    pub search_method: search::SearchMethod,
    pub mem_limit: u64,
    pub time_limit: u64,
    pub is_raw: bool,
    pub uses_asan: bool,
    pub ld_library: String,
    pub enable_afl: bool,
    pub enable_exploitation: bool,
    pub directed_targets_file: String,
    pub sanopt_bin: Option<String>,
    pub directed_only: bool,
}

pub fn make_absolute_str(path_str: &str) -> String {
    let path = Path::new(path_str);
    if path.is_relative() {
        let abs_path = env::current_dir().unwrap();
        return abs_path.join(path).to_str().unwrap().to_string();
    }
    path.to_path_buf().to_str().unwrap().to_string()
}

impl CommandOpt {
    pub fn new(
        mode: &str,
        track_target: &str,
        pargs: Vec<String>,
        out_dir: &Path,
        search_method: &str,
        mut mem_limit: u64,
        time_limit: u64,
        enable_afl: bool,
        enable_exploitation: bool,
        directed_targets_file: &str,
        sanopt_target: Option<&str>,
        directed_only: bool,
    ) -> Self {
        let mode = InstrumentationMode::from(mode);
        
        let tmp_dir = out_dir.join(TMP_DIR);
        tmpfs::create_tmpfs_dir(&tmp_dir);

        let out_file = tmp_dir.join(INPUT_FILE).to_str().unwrap().to_owned();
        let forksrv_socket_path = tmp_dir
            .join(FORKSRV_SOCKET_FILE)
            .to_str()
            .unwrap()
            .to_owned();

        let track_path = tmp_dir.join(TRACK_FILE).to_str().unwrap().to_owned();

        let has_input_arg = pargs.contains(&"@@".to_string());

        let clang_lib = Command::new("llvm-config")
            .arg("--libdir")
            .output()
            .expect("Can't find llvm-config")
            .stdout;
        let clang_lib = String::from_utf8(clang_lib).unwrap();
        let ld_library = "$LD_LIBRARY_PATH:".to_string() + clang_lib.trim();

        assert_ne!(
            track_target, "-",
            "You should set track target with -t PROM in LLVM mode!"
        );

        let mut tmp_args = pargs.clone();
        let main_bin = make_absolute_str(&tmp_args[0].clone());
        let main_args: Vec<String> = tmp_args.drain(1..).collect();
        let uses_asan = check_dep::check_asan(&main_bin);
        if uses_asan && mem_limit != 0 {
            warn!("The program compiled with ASAN, set MEM_LIMIT to 0 (unlimited)");
            mem_limit = 0;
        }

        let track_bin;
        let mut track_args = Vec::<String>::new();
        if mode.is_pin_mode() {
            let project_bin_dir = env::var(defs::ANGORA_BIN_DIR).expect("Please set ANGORA_PROJ_DIR");
            
            let pin_root =
                env::var(PIN_ROOT_VAR).expect("You should set the environment of PIN_ROOT!");
            let pin_bin = format!("{}/{}", pin_root, "pin");
            track_bin = pin_bin.to_string();
            let pin_tool = Path::new(&project_bin_dir)
                .join("lib")
                .join("pin_track.so")
                .to_str()
                .unwrap()
                .to_owned();

            track_args.push(String::from("-t"));
            track_args.push(pin_tool);
            track_args.push(String::from("--"));
            track_args.push(track_target.to_string());
            track_args.extend(main_args.clone());
        } else {
            track_bin = make_absolute_str(&track_target);
            track_args = main_args.clone();
        }
        let sanopt_bin = sanopt_target.map(|s| s.to_string());

        Self {
            mode,
            id: 0,
            main: (main_bin, main_args),
            track: (track_bin, track_args),
            tmp_dir,
            out_file: out_file,
            forksrv_socket_path,
            track_path,
            is_stdin: !has_input_arg,
            search_method: search::parse_search_method(search_method),
            mem_limit,
            time_limit,
            uses_asan,
            is_raw: true,
            ld_library,
            enable_afl,
            enable_exploitation,
            directed_targets_file: directed_targets_file.to_string(),
            sanopt_bin,
            directed_only,
        }
    }

    pub fn specify(&self, id: usize) -> Self {
        let mut cmd_opt = self.clone();
        let new_file = format!("{}_{}", &cmd_opt.out_file, id);
        let new_forksrv_socket_path = format!("{}_{}", &cmd_opt.forksrv_socket_path, id);
        let new_track_path = format!("{}_{}", &cmd_opt.track_path, id);
        if !self.is_stdin {
            for arg in &mut cmd_opt.main.1 {
                if arg == "@@" {
                    *arg = new_file.clone();
                }
            }
            for arg in &mut cmd_opt.track.1 {
                if arg == "@@" {
                    *arg = new_file.clone();
                }
            }
        }
        cmd_opt.id = id;
        cmd_opt.out_file = new_file.to_owned();
        cmd_opt.forksrv_socket_path = new_forksrv_socket_path.to_owned();
        cmd_opt.track_path = new_track_path.to_owned();
        cmd_opt.is_raw = false;
        cmd_opt
    }

    pub fn sanopt(&self) -> Self {
        let mut cmd_opt = self.clone();
        let main_args = self.main.1.clone();
        let bin = if let Some(optbin) = &self.sanopt_bin {
            optbin.to_string()
        } else {
            self.main.0.clone()
        };
        cmd_opt.main = (bin, main_args);
        let new_file =  format!("{}_{}", &self.out_file, "sanopt");
        let new_forksrv_socket_path = format!("{}_{}", &self.forksrv_socket_path, "sanopt");
        let new_track_path = format!("{}_{}", &cmd_opt.track_path, "sanopt");
        cmd_opt.out_file = new_file.to_owned();
        cmd_opt.forksrv_socket_path = new_forksrv_socket_path.to_owned();
        cmd_opt.track_path = new_track_path.to_owned();
        cmd_opt.uses_asan = true;
        cmd_opt.mem_limit = 0;
        cmd_opt
    }
}

impl Drop for CommandOpt {
    fn drop(&mut self) {
        if self.is_raw {
            tmpfs::clear_tmpfs_dir(&self.tmp_dir);
        }
    }
}
