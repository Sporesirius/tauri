// Copyright 2019-2021 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use anyhow::Context;
use tauri_bundler::bundle::{bundle_project, PackageType, SettingsBuilder};

use crate::helpers::{
  app_paths::{app_dir, tauri_dir},
  config::get as get_config,
  execute_with_output,
  manifest::rewrite_manifest,
  updater_signature::sign_file_from_env_variables,
  Logger,
};

use std::{env::set_current_dir, fs::rename, path::PathBuf, process::Command};

mod rust;

#[derive(Default)]
pub struct Build {
  runner: Option<String>,
  debug: bool,
  verbose: bool,
  target: Option<String>,
  bundles: Option<Vec<String>>,
  config: Option<String>,
}

impl Build {
  pub fn new() -> Self {
    Default::default()
  }

  pub fn debug(mut self) -> Self {
    self.debug = true;
    self
  }

  pub fn verbose(mut self) -> Self {
    self.verbose = true;
    self
  }

  pub fn runner(mut self, runner: String) -> Self {
    self.runner.replace(runner);
    self
  }

  pub fn target(mut self, target: String) -> Self {
    self.target.replace(target);
    self
  }

  pub fn bundles(mut self, bundles: Vec<String>) -> Self {
    self.bundles.replace(bundles);
    self
  }

  pub fn config(mut self, config: String) -> Self {
    self.config.replace(config);
    self
  }

  pub fn run(self) -> crate::Result<()> {
    let logger = Logger::new("tauri:build");
    let config = get_config(self.config.as_deref())?;

    let tauri_path = tauri_dir();
    set_current_dir(&tauri_path).with_context(|| "failed to change current working directory")?;

    let manifest = rewrite_manifest(config.clone())?;

    let config_guard = config.lock().unwrap();
    let config_ = config_guard.as_ref().unwrap();

    if let Some(before_build) = &config_.build.before_build_command {
      if !before_build.is_empty() {
        logger.log(format!("Running `{}`", before_build));
        #[cfg(target_os = "windows")]
        execute_with_output(
          &mut Command::new("cmd")
            .arg("/C")
            .arg(before_build)
            .current_dir(app_dir()),
        )
        .with_context(|| format!("failed to run `{}` with `cmd /C`", before_build))?;
        #[cfg(not(target_os = "windows"))]
        execute_with_output(
          &mut Command::new("sh")
            .arg("-c")
            .arg(before_build)
            .current_dir(app_dir()),
        )
        .with_context(|| format!("failed to run `{}` with `sh -c`", before_build))?;
      }
    }

    let web_asset_path = PathBuf::from(&config_.build.dist_dir);
    if !web_asset_path.exists() {
      return Err(anyhow::anyhow!(
        "Unable to find your web assets, did you forget to build your web app? Your distDir is set to \"{:?}\".",
        web_asset_path
      ));
    }

    let runner_from_config = config_.build.runner.clone();
    let runner = self
      .runner
      .or(runner_from_config)
      .unwrap_or_else(|| "cargo".to_string());

    rust::build_project(runner, &self.target, self.debug).with_context(|| "failed to build app")?;

    let app_settings = rust::AppSettings::new(&config_)?;

    let out_dir = app_settings
      .get_out_dir(self.debug)
      .with_context(|| "failed to get project out directory")?;
    if let Some(product_name) = config_.package.product_name.clone() {
      let bin_name = app_settings.cargo_package_settings().name.clone();
      #[cfg(windows)]
      rename(
        out_dir.join(format!("{}.exe", bin_name)),
        out_dir.join(format!("{}.exe", product_name)),
      )?;
      #[cfg(not(windows))]
      rename(out_dir.join(bin_name), out_dir.join(product_name))?;
    }

    if config_.tauri.bundle.active {
      // move merge modules to the out dir so the bundler can load it
      #[cfg(windows)]
      {
        let (filename, vcruntime_msm) = if cfg!(target_arch = "x86") {
          let _ = std::fs::remove_file(out_dir.join("Microsoft_VC142_CRT_x64.msm"));
          (
            "Microsoft_VC142_CRT_x86.msm",
            include_bytes!("../MergeModules/Microsoft_VC142_CRT_x86.msm").to_vec(),
          )
        } else {
          let _ = std::fs::remove_file(out_dir.join("Microsoft_VC142_CRT_x86.msm"));
          (
            "Microsoft_VC142_CRT_x64.msm",
            include_bytes!("../MergeModules/Microsoft_VC142_CRT_x64.msm").to_vec(),
          )
        };
        std::fs::write(out_dir.join(filename), vcruntime_msm)?;
      }
      let mut settings_builder = SettingsBuilder::new()
        .package_settings(app_settings.get_package_settings())
        .bundle_settings(app_settings.get_bundle_settings(&config_, &manifest)?)
        .binaries(app_settings.get_binaries(&config_)?)
        .project_out_directory(out_dir);

      if self.verbose {
        settings_builder = settings_builder.verbose();
      }

      if let Some(names) = self.bundles {
        let mut types = vec![];
        for name in names {
          if name == "none" {
            break;
          }
          match PackageType::from_short_name(&name) {
            Some(package_type) => {
              types.push(package_type);
            }
            None => {
              return Err(anyhow::anyhow!(format!(
                "Unsupported bundle format: {}",
                name
              )));
            }
          }
        }

        settings_builder = settings_builder.package_types(types);
      }

      // Bundle the project
      let settings = settings_builder
        .build()
        .with_context(|| "failed to build bundler settings")?;

      let bundles = bundle_project(settings).with_context(|| "failed to bundle project")?;

      // If updater is active and pubkey is available
      if config_.tauri.updater.active && config_.tauri.updater.pubkey.is_some() {
        // make sure we have our package builts
        let mut signed_paths = Vec::new();
        for elem in bundles
          .iter()
          .filter(|bundle| bundle.package_type == PackageType::Updater)
        {
          // we expect to have only one path in the vec but we iter if we add
          // another type of updater package who require multiple file signature
          for path in elem.bundle_paths.iter() {
            // sign our path from environment variables
            let (signature_path, _signature) = sign_file_from_env_variables(path)?;
            signed_paths.append(&mut vec![signature_path]);
          }
        }
        if !signed_paths.is_empty() {
          print_signed_updater_archive(&signed_paths)?;
        }
      }
    }

    Ok(())
  }
}

fn print_signed_updater_archive(output_paths: &[PathBuf]) -> crate::Result<()> {
  let pluralised = if output_paths.len() == 1 {
    "updater archive"
  } else {
    "updater archives"
  };
  let msg = format!("{} {} at:", output_paths.len(), pluralised);
  let logger = Logger::new("Signed");
  logger.log(&msg);
  for path in output_paths {
    println!("        {}", path.display());
  }
  Ok(())
}
