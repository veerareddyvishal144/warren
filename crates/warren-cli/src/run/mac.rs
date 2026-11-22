//! `warren mac init`: build the macOS base VM that `warrend` clones for every
//! `mac` instance, on an Apple silicon Mac with Lume 0.7.2 or newer.
//!
//! The build itself is `scripts/mac/build-base.sh` with the guest files from
//! `scripts/mac/guest/`, embedded in this binary at build time and written to
//! a temporary directory at run time in the layout the script expects
//! (`build-base.sh` next to a `guest/` directory). Without `--source` a stock
//! VM is first created from Apple's restore image with `lume create
//! --unattended <preset>`, which gives the guest the user `lume` (password
//! `lume`) with SSH enabled, the state `build-base.sh` starts from; on a Lume
//! without `--unattended` the one-time interactive steps are printed instead.
//! `--dry-run` prints every step and command and runs nothing.

use std::{
    fs,
    io::{IsTerminal, Write as _},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use serde_json::json;
use warren_cli::{
    EXIT_OK, Failure,
    cli::{MacCmd, MacInitArgs},
    util::shell_join,
};

use super::{
    Ctx,
    setup::{self, LUME_INSTALL_HINT, LUME_MIN_VERSION},
};

const BUILD_BASE_SH: &str = include_str!("../../../../scripts/mac/build-base.sh");
/// `scripts/mac/guest/*`: name, content, executable.
const GUEST_FILES: [(&str, &str, bool); 5] = [
    (
        "install.sh",
        include_str!("../../../../scripts/mac/guest/install.sh"),
        true,
    ),
    (
        "warren-gateway-launcher.sh",
        include_str!("../../../../scripts/mac/guest/warren-gateway-launcher.sh"),
        true,
    ),
    (
        "dev.warren.gateway.plist",
        include_str!("../../../../scripts/mac/guest/dev.warren.gateway.plist"),
        false,
    ),
    (
        "dev.warren.pf.plist",
        include_str!("../../../../scripts/mac/guest/dev.warren.pf.plist"),
        false,
    ),
    (
        "pf.anchor.template",
        include_str!("../../../../scripts/mac/guest/pf.anchor.template"),
        false,
    ),
];

/// Shape of the stock VM created without `--source` (docs/macos.md).
const STOCK_CPU: &str = "4";
const STOCK_MEMORY: &str = "4GB";
const STOCK_DISK: &str = "50GB";

/// `warren mac ...`
pub fn run(ctx: &Ctx, cmd: MacCmd) -> Result<i32, Failure> {
    match cmd {
        MacCmd::Init(args) => init(ctx, args),
    }
}

/// What the plan depends on, probed before anything runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    /// `lume` and its version, when installed.
    pub lume: Option<(PathBuf, String)>,
    /// `lume create --unattended` exists.
    pub unattended: bool,
    /// Status of the source VM (`running`, `stopped`), `None` when it does not exist.
    pub source_status: Option<String>,
    /// Status of the target VM, `None` when it does not exist.
    pub target_status: Option<String>,
    /// The SSH key `build-base.sh` installs in the guest.
    pub ssh_key: PathBuf,
    pub ssh_key_exists: bool,
    pub is_apple_silicon: bool,
}

impl Facts {
    fn probe(args: &MacInitArgs, home: &Path) -> Facts {
        let lume = setup::lume_version();
        let unattended = lume
            .as_ref()
            .map(|(bin, _)| {
                Command::new(bin)
                    .args(["create", "--help"])
                    .stdin(Stdio::null())
                    .output()
                    .map(|o| {
                        String::from_utf8_lossy(&o.stdout).contains("--unattended")
                            || String::from_utf8_lossy(&o.stderr).contains("--unattended")
                    })
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        let source = source_name(args);
        let status = |name: &str| {
            lume.as_ref()
                .and_then(|(bin, _)| setup::lume_vm_status(bin, name))
        };
        let ssh_key = home.join(".ssh/id_ed25519");
        Facts {
            source_status: status(source),
            target_status: status(&args.target),
            unattended,
            lume,
            ssh_key_exists: ssh_key.is_file(),
            ssh_key,
            is_apple_silicon: cfg!(target_os = "macos") && std::env::consts::ARCH == "aarch64",
        }
    }
}

/// One thing `init` does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Run a command with the terminal attached.
    Run { title: String, argv: Vec<String> },
    /// Write the embedded scripts into the staging directory.
    Stage { dir: PathBuf },
    /// Remove the staging directory.
    Cleanup { dir: PathBuf },
    /// Something the user has to do by hand; wait for Enter afterwards.
    Manual {
        title: String,
        instructions: Vec<String>,
    },
}

impl Step {
    fn describe(&self) -> String {
        match self {
            Step::Run { title, argv } => format!("{title}\n       {}", shell_join(argv)),
            Step::Stage { dir } => format!(
                "stage the build scripts in {} (build-base.sh, guest/{})",
                dir.display(),
                GUEST_FILES
                    .iter()
                    .map(|(n, _, _)| *n)
                    .collect::<Vec<_>>()
                    .join(", guest/")
            ),
            Step::Cleanup { dir } => format!("remove {}", dir.display()),
            Step::Manual {
                title,
                instructions,
            } => {
                let mut s = format!("{title} (by hand, then press Enter):");
                for i in instructions {
                    s.push_str(&format!("\n       - {i}"));
                }
                s
            }
        }
    }
}

/// The source VM: `--source`, else the stock VM.
fn source_name(args: &MacInitArgs) -> &str {
    args.source.as_deref().unwrap_or(&args.stock)
}

/// Notes printed above the steps (what was found).
pub fn summary(args: &MacInitArgs, facts: &Facts) -> Vec<String> {
    let mut out = Vec::new();
    match &facts.lume {
        Some((bin, v)) => out.push(format!(
            "lume      {v} at {}{}",
            bin.display(),
            if facts.unattended {
                ", `lume create --unattended` available"
            } else {
                ", no `lume create --unattended` (manual first boot)"
            }
        )),
        None => out.push("lume      not installed".to_string()),
    }
    let source = source_name(args);
    out.push(match (&args.source, &facts.source_status) {
        (Some(_), Some(st)) => format!("source    {source}: exists, {st}"),
        (Some(_), None) => format!("source    {source}: DOES NOT EXIST (lume ls)"),
        (None, Some(st)) => format!("source    {source}: exists, {st} (reused as the stock VM)"),
        (None, None) => format!(
            "source    {source}: to be created from the restore image `{}` with the `{}` preset",
            args.ipsw, args.unattended
        ),
    });
    out.push(match &facts.target_status {
        Some(st) if args.force => format!(
            "target    {}: exists ({st}); --force replaces it",
            args.target
        ),
        Some(st) => format!(
            "target    {}: exists ({st}); build-base.sh refuses to overwrite it without --force",
            args.target
        ),
        None => format!("target    {}: to be created", args.target),
    });
    out.push(format!(
        "ssh key   {}{}",
        facts.ssh_key.display(),
        if facts.ssh_key_exists {
            ""
        } else {
            " (missing; will be generated)"
        }
    ));
    if args.with_agents {
        out.push("agents    Node LTS, claude, codex and opencode go into the image (about 1 GB through the VM's NAT)".to_string());
    }
    out
}

/// The steps for `args` given `facts`.
pub fn plan(args: &MacInitArgs, facts: &Facts, stage: &Path) -> Vec<Step> {
    let mut steps = Vec::new();
    let source = source_name(args).to_string();
    let lume = facts
        .lume
        .as_ref()
        .map(|(b, _)| b.display().to_string())
        .unwrap_or_else(|| "lume".to_string());
    if !facts.ssh_key_exists {
        steps.push(Step::Run {
            title: "generate the SSH key build-base.sh installs in the guest".into(),
            argv: vec![
                "ssh-keygen".into(),
                "-t".into(),
                "ed25519".into(),
                "-f".into(),
                facts.ssh_key.display().to_string(),
                "-N".into(),
                String::new(),
            ],
        });
    }
    if args.source.is_none() && facts.source_status.is_none() {
        let mut argv = vec![
            lume.clone(),
            "create".into(),
            source.clone(),
            "--os".into(),
            "macos".into(),
            "--cpu".into(),
            STOCK_CPU.into(),
            "--memory".into(),
            STOCK_MEMORY.into(),
            "--disk-size".into(),
            STOCK_DISK.into(),
            "--ipsw".into(),
            args.ipsw.clone(),
        ];
        if facts.unattended {
            argv.push("--unattended".into());
            argv.push(args.unattended.clone());
            steps.push(Step::Run {
                title: format!(
                    "create the stock VM {source} from Apple's restore image (`latest` downloads it, about 15 GB, into ~/.lume/cache); the unattended setup creates the user lume (password lume) with SSH on, boots once to verify and stops"
                ),
                argv,
            });
        } else {
            steps.push(Step::Run {
                title: format!("create the stock VM {source} from Apple's restore image"),
                argv,
            });
            steps.push(Step::Manual {
                title: format!("finish macOS setup in {source}"),
                instructions: vec![
                    format!("{lume} run {source}   (opens the VM window)"),
                    "in Setup Assistant create the user `lume` with the password `lume` (build-base.sh logs in with it once, then replaces it)".into(),
                    "System Settings > General > Sharing: turn Remote Login (SSH) on".into(),
                    format!("shut the guest down from the Apple menu, or `{lume} stop {source}`"),
                ],
            });
        }
    } else if facts.source_status.as_deref() == Some("running") {
        steps.push(Step::Run {
            title: format!("stop {source}: build-base.sh clones a stopped VM"),
            argv: vec![lume.clone(), "stop".into(), source.clone()],
        });
    }
    steps.push(Step::Stage {
        dir: stage.to_path_buf(),
    });
    let mut argv = vec![
        "bash".to_string(),
        stage.join("build-base.sh").display().to_string(),
        "--source".into(),
        source,
        "--target".into(),
        args.target.clone(),
    ];
    if args.force {
        argv.push("--force".into());
    }
    if args.with_agents {
        argv.push("--with-agents".into());
    }
    if args.auto_login {
        argv.push("--auto-login".into());
    }
    steps.push(Step::Run {
        title: format!(
            "clone the source into {} and install the gateway launcher, pf rules, your SSH key and a new guest password (stored in ~/.warren/mac/{}.password)",
            args.target, args.target
        ),
        argv,
    });
    steps.push(Step::Cleanup {
        dir: stage.to_path_buf(),
    });
    steps
}

/// Write `build-base.sh` and `guest/*` into `dir`.
pub fn stage_scripts(dir: &Path) -> Result<(), Failure> {
    use std::os::unix::fs::PermissionsExt;
    let guest = dir.join("guest");
    fs::create_dir_all(&guest).map_err(|e| Failure::local(format!("{}: {e}", guest.display())))?;
    let write = |path: &Path, body: &str, exec: bool| -> Result<(), Failure> {
        fs::write(path, body).map_err(|e| Failure::local(format!("{}: {e}", path.display())))?;
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if exec { 0o755 } else { 0o644 }),
        )
        .map_err(|e| Failure::local(format!("{}: {e}", path.display())))
    };
    write(&dir.join("build-base.sh"), BUILD_BASE_SH, true)?;
    for (name, body, exec) in GUEST_FILES {
        write(&guest.join(name), body, exec)?;
    }
    Ok(())
}

/// `warren mac init`
fn init(ctx: &Ctx, args: MacInitArgs) -> Result<i32, Failure> {
    let home = setup::home_dir()?;
    let facts = Facts::probe(&args, &home);
    let stage = std::env::temp_dir().join(format!("warren-mac-init-{}", std::process::id()));
    let steps = plan(&args, &facts, &stage);
    let notes = summary(&args, &facts);

    if ctx.json && args.dry_run {
        println!(
            "{}",
            warren_cli::output::to_json_pretty(&json!({
                "dry_run": true,
                "facts": {
                    "lume": facts.lume.as_ref().map(|(b, v)| json!({"path": b, "version": v})),
                    "unattended": facts.unattended,
                    "source": source_name(&args),
                    "source_status": facts.source_status,
                    "target": args.target,
                    "target_status": facts.target_status,
                    "ssh_key": facts.ssh_key,
                    "ssh_key_exists": facts.ssh_key_exists,
                },
                "steps": steps.iter().map(|s| match s {
                    Step::Run { title, argv } => json!({"run": argv, "title": title}),
                    Step::Stage { dir } => json!({"stage": dir}),
                    Step::Cleanup { dir } => json!({"cleanup": dir}),
                    Step::Manual { title, instructions } => json!({"manual": instructions, "title": title}),
                }).collect::<Vec<_>>(),
            }))
        );
        return Ok(EXIT_OK);
    }

    println!(
        "warren mac init{}",
        if args.dry_run {
            " (dry run: nothing below is executed)"
        } else {
            ""
        }
    );
    for n in &notes {
        println!("  {n}");
    }
    println!("Steps:");
    for (i, s) in steps.iter().enumerate() {
        println!("  {}. {}", i + 1, s.describe());
    }
    println!(
        "Afterwards: `warren up` again so the daemon picks up {}, then `warren run --type mac -- sw_vers` or `warren instances create --template warren-macos --wait`.",
        args.target
    );
    if args.dry_run {
        return Ok(EXIT_OK);
    }

    // Preconditions for a real run.
    if !facts.is_apple_silicon {
        return Err(Failure::local(
            "macOS virtual machines need an Apple silicon Mac (Virtualization.framework); this is not one",
        ));
    }
    let min = format!(
        "{}.{}.{}",
        LUME_MIN_VERSION.0, LUME_MIN_VERSION.1, LUME_MIN_VERSION.2
    );
    match &facts.lume {
        None => {
            return Err(Failure::local(format!(
                "lume is not installed; install {min} or newer with:\n  {LUME_INSTALL_HINT}\nthen run `warren mac init` again"
            )));
        }
        Some((_, v)) if !setup::version_at_least(v, LUME_MIN_VERSION) => {
            return Err(Failure::local(format!(
                "lume {v} is too old; {min} or newer is required:\n  {LUME_INSTALL_HINT}"
            )));
        }
        Some(_) => {}
    }
    if args.source.is_some() && facts.source_status.is_none() {
        return Err(Failure::local(format!(
            "--source {}: no such VM (`lume ls` lists them); leave --source out to create a stock VM from a restore image",
            source_name(&args)
        )));
    }
    if facts.target_status.is_some() && !args.force {
        return Err(Failure::local(format!(
            "the target VM {} already exists; pass --force to replace it or --target NAME for another name",
            args.target
        )));
    }
    if args.ipsw != "latest" && args.source.is_none() && facts.source_status.is_none() {
        let p = Path::new(&args.ipsw);
        if !p.is_file() {
            return Err(Failure::local(format!(
                "--ipsw {}: no such file (use `latest` to let Lume download the current restore image)",
                args.ipsw
            )));
        }
    }

    let _guard = StageGuard(stage.clone());
    for (i, step) in steps.iter().enumerate() {
        eprintln!(
            "[mac init] step {}/{}: {}",
            i + 1,
            steps.len(),
            first_line(&step.describe())
        );
        match step {
            Step::Stage { dir } => stage_scripts(dir)?,
            Step::Cleanup { dir } => {
                let _ = fs::remove_dir_all(dir);
            }
            Step::Run { argv, .. } => {
                let status = Command::new(&argv[0])
                    .args(&argv[1..])
                    .status()
                    .map_err(|e| Failure::local(format!("{}: {e}", argv[0])))?;
                if !status.success() {
                    return Err(Failure::local(format!(
                        "`{}` failed with {}; fix the problem above and run `warren mac init` again",
                        shell_join(argv),
                        status
                    )));
                }
            }
            Step::Manual { instructions, .. } => {
                if !std::io::stdin().is_terminal() {
                    return Err(Failure::local(
                        "this step needs a terminal to confirm: run `warren mac init` interactively",
                    ));
                }
                for i in instructions {
                    println!("  - {i}");
                }
                print!("Press Enter when the VM is set up and stopped... ");
                let _ = std::io::stdout().flush();
                let mut line = String::new();
                std::io::stdin()
                    .read_line(&mut line)
                    .map_err(|e| Failure::local(e.to_string()))?;
            }
        }
    }
    println!(
        "Base VM {} is ready. Run `warren up` so the daemon picks it up (it clones it for every mac instance).",
        args.target
    );
    Ok(EXIT_OK)
}

/// Removes the staging directory on every exit path.
struct StageGuard(PathBuf);

impl Drop for StageGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(source: Option<&str>) -> MacInitArgs {
        MacInitArgs {
            source: source.map(str::to_string),
            ipsw: "latest".into(),
            stock: "base".into(),
            unattended: "tahoe".into(),
            target: "warren-macos-26".into(),
            with_agents: false,
            auto_login: false,
            force: false,
            dry_run: true,
        }
    }

    fn facts() -> Facts {
        Facts {
            lume: Some((PathBuf::from("/usr/local/bin/lume"), "0.7.2".into())),
            unattended: true,
            source_status: Some("stopped".into()),
            target_status: None,
            ssh_key: PathBuf::from("/Users/u/.ssh/id_ed25519"),
            ssh_key_exists: true,
            is_apple_silicon: true,
        }
    }

    fn runs(steps: &[Step]) -> Vec<Vec<String>> {
        steps
            .iter()
            .filter_map(|s| match s {
                Step::Run { argv, .. } => Some(argv.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn plan_with_an_existing_source_only_runs_build_base() {
        let stage = Path::new("/tmp/stage");
        let steps = plan(&args(Some("sandbox")), &facts(), stage);
        assert_eq!(steps.len(), 3);
        assert_eq!(
            steps[0],
            Step::Stage {
                dir: stage.to_path_buf()
            }
        );
        let r = runs(&steps);
        assert_eq!(r.len(), 1);
        assert_eq!(
            r[0],
            vec![
                "bash",
                "/tmp/stage/build-base.sh",
                "--source",
                "sandbox",
                "--target",
                "warren-macos-26"
            ]
        );
        assert_eq!(
            steps[2],
            Step::Cleanup {
                dir: stage.to_path_buf()
            }
        );
        let mut a = args(Some("sandbox"));
        a.force = true;
        a.with_agents = true;
        let r = runs(&plan(&a, &facts(), stage));
        assert!(r[0].ends_with(&["--force".to_string(), "--with-agents".to_string()]));
    }

    #[test]
    fn plan_creates_the_stock_vm_unattended() {
        let mut f = facts();
        f.source_status = None;
        let steps = plan(&args(None), &f, Path::new("/tmp/stage"));
        let r = runs(&steps);
        assert_eq!(r.len(), 2);
        assert_eq!(
            r[0],
            vec![
                "/usr/local/bin/lume",
                "create",
                "base",
                "--os",
                "macos",
                "--cpu",
                "4",
                "--memory",
                "4GB",
                "--disk-size",
                "50GB",
                "--ipsw",
                "latest",
                "--unattended",
                "tahoe"
            ]
        );
        assert!(r[1].contains(&"--source".to_string()) && r[1].contains(&"base".to_string()));
        assert!(!steps.iter().any(|s| matches!(s, Step::Manual { .. })));
        // Without --unattended support the first boot is manual.
        f.unattended = false;
        let steps = plan(&args(None), &f, Path::new("/tmp/stage"));
        assert!(!runs(&steps)[0].contains(&"--unattended".to_string()));
        assert!(steps.iter().any(|s| matches!(s, Step::Manual { instructions, .. } if instructions.iter().any(|i| i.contains("Remote Login")))));
        // An existing stock VM is reused; a running one is stopped first.
        f.source_status = Some("running".into());
        let r = runs(&plan(&args(None), &f, Path::new("/tmp/stage")));
        assert_eq!(r[0], vec!["/usr/local/bin/lume", "stop", "base"]);
    }

    #[test]
    fn missing_ssh_key_is_generated_first() {
        let mut f = facts();
        f.ssh_key_exists = false;
        let r = runs(&plan(&args(Some("sandbox")), &f, Path::new("/tmp/stage")));
        assert_eq!(r[0][0], "ssh-keygen");
        assert!(r[0].contains(&"/Users/u/.ssh/id_ed25519".to_string()));
    }

    #[test]
    fn staging_writes_the_layout_build_base_expects() {
        let dir = std::env::temp_dir().join(format!(
            "warren-cli-mac-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        stage_scripts(&dir).unwrap();
        let script = fs::read_to_string(dir.join("build-base.sh")).unwrap();
        assert!(script.contains("GUEST_DIR=\"$HERE/guest\""));
        for (name, body, exec) in GUEST_FILES {
            let p = dir.join("guest").join(name);
            assert_eq!(fs::read_to_string(&p).unwrap(), body, "{name}");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o111;
                assert_eq!(mode != 0, exec, "{name}");
            }
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn summary_mentions_what_matters() {
        let mut a = args(Some("sandbox"));
        let mut f = facts();
        f.target_status = Some("stopped".into());
        let s = summary(&a, &f).join("\n");
        assert!(s.contains("lume      0.7.2"));
        assert!(s.contains("source    sandbox: exists, stopped"));
        assert!(s.contains("without --force"));
        a.force = true;
        assert!(summary(&a, &f).join("\n").contains("--force replaces it"));
        f.source_status = None;
        assert!(summary(&a, &f).join("\n").contains("DOES NOT EXIST"));
        let s = summary(&args(None), &f).join("\n");
        assert!(s.contains("to be created from the restore image `latest`"));
    }
}
