use std::collections::BTreeMap;
use std::env::{join_paths, split_paths, temp_dir};
use std::path::{Path, PathBuf};
use std::process::exit;
use std::sync::mpsc;
use std::time::Duration;
use std::{fs, thread};

use clap::Command;
use color_eyre::eyre::{Context, Result};

use crate::cache::CacheManager;
use crate::cmd::CmdLineRunner;
use crate::config::{Config, Settings};
use crate::env::{
    RTX_EXE, RTX_FETCH_REMOTE_VERSIONS_TIMEOUT, RTX_NODE_CONCURRENCY, RTX_NODE_FORCE_COMPILE,
    RTX_NODE_VERBOSE_INSTALL,
};
use crate::file::{create_dir_all, remove_all};
use crate::git::Git;
use crate::lock_file::LockFile;
use crate::plugins::{Plugin, PluginName};
use crate::toolset::{ToolVersion, ToolVersionRequest};
use crate::ui::progress_report::ProgressReport;
use crate::{cmd, dirs, env, file};

#[derive(Debug)]
pub struct RubyPlugin {
    pub name: PluginName,
    cache_path: PathBuf,
    remote_version_cache: CacheManager<Vec<String>>,
    legacy_file_support: bool,
}

impl RubyPlugin {
    pub fn new(name: PluginName) -> Self {
        let cache_path = dirs::CACHE.join(&name);
        let fresh_duration = Some(Duration::from_secs(60 * 60 * 12)); // 12 hours
        Self {
            remote_version_cache: CacheManager::new(cache_path.join("remote_versions.msgpack.z"))
                .with_fresh_duration(fresh_duration)
                .with_fresh_file(RTX_EXE.clone()),
            name,
            cache_path,
            legacy_file_support: true,
        }
    }

    fn ruby_build_path(&self) -> PathBuf {
        self.cache_path.join("ruby-build")
    }
    fn ruby_build_bin(&self) -> PathBuf {
        self.ruby_build_path().join("bin/ruby-build")
    }
    fn install_or_update_ruby_build(&self) -> Result<()> {
        let _lock = self.lock_ruby_build();
        if self.ruby_build_path().exists() {
            self.update_ruby_build()
        } else {
            self.install_ruby_build()
        }
    }

    fn lock_ruby_build(&self) -> Result<fslock::LockFile, std::io::Error> {
        LockFile::new(&self.ruby_build_path())
            .with_callback(|l| {
                trace!("install_or_update_ruby_build {}", l.display());
            })
            .lock()
    }
    fn install_ruby_build(&self, config: &Config) -> Result<()> {
        if self.ruby_build_path().exists() {
            return Ok(());
        }
        debug!(
            "Installing ruby-build to {}",
            self.ruby_build_path().display()
        );
        let tmp = temp_dir().join("rtx-ruby-build");
        create_dir_all(&tmp.parent().unwrap())?;
        let git = Git::new(tmp);
        git.clone("https://github.com/rbenv/ruby-build.git")?;

        let mut cmd = CmdLineRunner::new(&)
        Ok(())
    }
    fn update_ruby_build(&self) -> Result<()> {
        // TODO: check if an update is available first
        debug!(
            "Updating ruby-build in {}",
            self.ruby_build_path().display()
        );
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            remove_all(self.ruby_build_path()).expect("error removing ruby-build");
            let result = self.install_ruby_build();
            tx.send(result).unwrap();
        });
        rx.recv_timeout(*RTX_FETCH_REMOTE_VERSIONS_TIMEOUT)
            .context("timed out updating ruby-build")??;
        Ok(())
    }

    fn fetch_remote_versions(&self) -> Result<Vec<String>> {
        self.install_or_update_ruby_build()?;
        let ruby_build_bin = self.ruby_build_bin();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let output = cmd!(ruby_build_bin, "--definitions").read();
            tx.send(output).unwrap();
        });
        let output = rx
            .recv_timeout(*RTX_FETCH_REMOTE_VERSIONS_TIMEOUT)
            .context("timed out fetching ruby versions")??;
        let versions = output
            .split('\n')
            .filter(|s| regex!(r"^[0-9].+$").is_match(s))
            .map(|s| s.to_string())
            .collect();
        Ok(versions)
    }

    fn ruby_path(&self, tv: &ToolVersion) -> PathBuf {
        tv.install_path().join("bin/ruby")
    }

    fn gem_path(&self, tv: &ToolVersion) -> PathBuf {
        tv.install_path().join("bin/gem")
    }

    fn install_default_packages(
        &self,
        settings: &Settings,
        tv: &ToolVersion,
        pr: &ProgressReport,
    ) -> Result<()> {
        let body = fs::read_to_string(&*env::RTX_NODE_DEFAULT_PACKAGES_FILE).unwrap_or_default();
        for package in body.lines() {
            let package = package.split('#').next().unwrap_or_default().trim();
            if package.is_empty() {
                continue;
            }
            pr.set_message(format!("installing default package: {}", package));
            let gem = self.gem_path(tv);
            let mut cmd = CmdLineRunner::new(settings, gem);
            cmd.with_pr(pr).arg("install").arg("--global").arg(package);
            let mut path = split_paths(&env::var_os("PATH").unwrap()).collect::<Vec<_>>();
            path.insert(0, tv.install_path().join("bin"));
            cmd.env("PATH", join_paths(path)?);
            cmd.execute()?;
        }
        Ok(())
    }

    fn install_gem_shim(&self, tv: &ToolVersion) -> Result<()> {
        fs::remove_file(self.gem_path(tv)).ok();
        fs::write(self.gem_path(tv), include_str!("assets/ruby_gem_shim"))?;
        file::make_executable(&self.gem_path(tv))?;
        Ok(())
    }

    fn test_ruby(&self, config: &Config, tv: &ToolVersion, pr: &ProgressReport) -> Result<()> {
        let mut cmd = CmdLineRunner::new(&config.settings, self.ruby_path(tv));
        cmd.with_pr(pr).arg("-v");
        cmd.execute()
    }

    fn test_gem(&self, config: &Config, tv: &ToolVersion, pr: &ProgressReport) -> Result<()> {
        let mut cmd = CmdLineRunner::new(&config.settings, self.gem_path(tv));
        let mut path = split_paths(&env::var_os("PATH").unwrap()).collect::<Vec<_>>();
        path.insert(0, tv.install_path().join("bin"));
        cmd.env("PATH", join_paths(path)?);
        cmd.with_pr(pr).arg("-v");
        cmd.execute()
    }
}

impl Plugin for RubyPlugin {
    fn name(&self) -> &PluginName {
        &self.name
    }

    fn list_remote_versions(&self, _settings: &Settings) -> Result<Vec<String>> {
        self.remote_version_cache
            .get_or_try_init(|| self.fetch_remote_versions())
            .cloned()
    }

    fn get_aliases(&self, _settings: &Settings) -> Result<BTreeMap<String, String>> {
        let aliases = [
            ("lts/argon", "4"),
            ("lts/boron", "6"),
            ("lts/carbon", "8"),
            ("lts/dubnium", "10"),
            ("lts/erbium", "12"),
            ("lts/fermium", "14"),
            ("lts/gallium", "16"),
            ("lts/hydrogen", "18"),
            ("lts-argon", "4"),
            ("lts-boron", "6"),
            ("lts-carbon", "8"),
            ("lts-dubnium", "10"),
            ("lts-erbium", "12"),
            ("lts-fermium", "14"),
            ("lts-gallium", "16"),
            ("lts-hydrogen", "18"),
            ("lts", "18"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        Ok(aliases)
    }

    fn legacy_filenames(&self, _settings: &Settings) -> Result<Vec<String>> {
        if self.legacy_file_support {
            Ok(vec![".ruby-version".into(), ".nvmrc".into()])
        } else {
            Ok(vec![])
        }
    }

    fn parse_legacy_file(&self, path: &Path, _settings: &Settings) -> Result<String> {
        let body = fs::read_to_string(path)?;
        // trim "v" prefix
        Ok(body.trim().strip_prefix('v').unwrap_or(&body).to_string())
    }

    fn external_commands(&self) -> Result<Vec<Command>> {
        if self.legacy_file_support {
            // sort of a hack to get this not to display for rubyjs
            let topic = Command::new("ruby")
                .about("Commands for the ruby plugin")
                .subcommands(vec![Command::new("ruby-build")
                    .about("Use/manage rtx's internal ruby-build")
                    .arg(
                        clap::Arg::new("args")
                            .num_args(1..)
                            .allow_hyphen_values(true)
                            .trailing_var_arg(true),
                    )]);
            Ok(vec![topic])
        } else {
            Ok(vec![])
        }
    }

    fn execute_external_command(
        &self,
        _config: &Config,
        command: &str,
        args: Vec<String>,
    ) -> Result<()> {
        match command {
            "ruby-build" => {
                self.install_or_update_ruby_build()?;
                cmd::cmd(self.ruby_build_bin(), args).run()?;
            }
            _ => unreachable!(),
        }
        exit(0);
    }

    fn install_version(
        &self,
        config: &Config,
        tv: &ToolVersion,
        pr: &ProgressReport,
    ) -> Result<()> {
        self.install_ruby_build()?;
        pr.set_message("running ruby-build");
        let mut cmd = CmdLineRunner::new(&config.settings, self.ruby_build_bin());
        cmd.with_pr(pr).arg(tv.version.as_str());
        if matches!(&tv.request, ToolVersionRequest::Ref { .. }) || *RTX_NODE_FORCE_COMPILE {
            let make_opts = String::from(" -j") + &RTX_NODE_CONCURRENCY.to_string();
            cmd.env(
                "MAKE_OPTS",
                env::var("MAKE_OPTS").unwrap_or_default() + &make_opts,
            );
            cmd.env(
                "NODE_MAKE_OPTS",
                env::var("NODE_MAKE_OPTS").unwrap_or_default() + &make_opts,
            );
            cmd.arg("--compile");
        }
        if config.settings.verbose || *RTX_NODE_VERBOSE_INSTALL {
            cmd.arg("--verbose");
        }
        cmd.arg(tv.install_path());
        cmd.execute()?;
        self.test_ruby(config, tv, pr)?;
        self.install_gem_shim(tv)?;
        self.test_gem(config, tv, pr)?;
        self.install_default_packages(&config.settings, tv, pr)?;
        Ok(())
    }
}
