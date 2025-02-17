use crate::scarb::config::SCARB_MANIFEST_TEMPLATE_CONTENT;
use crate::{NewArgs, Template, CAIRO_EDITION};
use anyhow::{anyhow, bail, ensure, Context, Ok, Result};
use camino::Utf8PathBuf;
use include_dir::{include_dir, Dir, DirEntry};
use indoc::formatdoc;
use scarb_api::ScarbCommand;
use semver::Version;
use shared::consts::FREE_RPC_PROVIDER_URL;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, Value, value};

static TEMPLATES_DIR: Dir = include_dir!("snforge_templates");

const DEFAULT_ASSERT_MACROS: Version = Version::new(2, 8, 5);
const MINIMAL_SCARB_FOR_CORRESPONDING_ASSERT_MACROS: Version = Version::new(2, 8, 0);

fn create_snfoundry_manifest(path: &PathBuf) -> Result<()> {
    fs::write(
        path,
        formatdoc! {r#"
        # Visit https://foundry-rs.github.io/starknet-foundry/appendix/snfoundry-toml.html
        # and https://foundry-rs.github.io/starknet-foundry/projects/configuration.html for more information

        # [sncast.default]                                         # Define a profile name
        # url = "{default_rpc_url}" # Url of the RPC provider
        # accounts-file = "../account-file"                        # Path to the file with the account data
        # account = "mainuser"                                     # Account from `accounts_file` or default account file that will be used for the transactions
        # keystore = "~/keystore"                                  # Path to the keystore file
        # wait-params = {{ timeout = 300, retry-interval = 10 }}     # Wait for submitted transaction parameters
        # block-explorer = "StarkScan"                             # Block explorer service used to display links to transaction details
        # show-explorer-links = true                               # Print links pointing to pages with transaction details in the chosen block explorer
        "#,
            default_rpc_url = FREE_RPC_PROVIDER_URL,
        },
    )?;

    Ok(())
}

fn add_template_to_scarb_manifest(path: &PathBuf) -> Result<()> {
    if !path.exists() {
        bail!("Scarb.toml not found");
    }

    let mut file = OpenOptions::new()
        .append(true)
        .open(path)
        .context("Failed to open Scarb.toml")?;

    file.write_all(SCARB_MANIFEST_TEMPLATE_CONTENT.as_bytes())
        .context("Failed to write to Scarb.toml")?;
    Ok(())
}

fn overwrite_or_copy_files(
    dir: &Dir,
    template_path: &Path,
    project_path: &Path,
    project_name: &str,
) -> Result<()> {
    for entry in dir.entries() {
        let path_without_template_name = entry.path().strip_prefix(template_path)?;
        let destination = project_path.join(path_without_template_name);
        match entry {
            DirEntry::Dir(dir) => {
                fs::create_dir_all(&destination)?;
                overwrite_or_copy_files(dir, template_path, project_path, project_name)?;
            }
            DirEntry::File(file) => {
                let contents = file.contents();
                let contents = replace_project_name(contents, project_name)?;
                fs::write(destination, contents)?;
            }
        }
    }

    Ok(())
}

fn replace_project_name(contents: &[u8], project_name: &str) -> Result<Vec<u8>> {
    let contents = std::str::from_utf8(contents).context("UTF-8 error")?;
    let contents = contents.replace("{{ PROJECT_NAME }}", project_name);
    Ok(contents.into_bytes())
}

fn update_config(config_path: &Path, template: &Template) -> Result<()> {
    let config_file = fs::read_to_string(config_path)?;
    let mut document = config_file
        .parse::<DocumentMut>()
        .context("invalid document")?;

    if !matches!(template, Template::CairoProgram) {
        add_target_to_toml(&mut document);
    }

    set_cairo_edition(&mut document, CAIRO_EDITION);
    add_test_script(&mut document);
    add_assert_macros(&mut document)?;
    add_allow_prebuilt_macros(&mut document)?;

    fs::write(config_path, document.to_string())?;

    Ok(())
}

fn add_test_script(document: &mut DocumentMut) {
    let mut test = Table::new();

    test.insert("test", value("snforge test"));
    document.insert("scripts", Item::Table(test));
}

fn add_target_to_toml(document: &mut DocumentMut) {
    let mut array_of_tables = ArrayOfTables::new();
    let mut sierra = Table::new();
    let mut contract = Table::new();
    contract.set_implicit(true);

    sierra.insert("sierra", Item::Value(true.into()));
    array_of_tables.push(sierra);
    contract.insert("starknet-contract", Item::ArrayOfTables(array_of_tables));

    document.insert("target", Item::Table(contract));
}

fn set_cairo_edition(document: &mut DocumentMut, cairo_edition: &str) {
    document["package"]["edition"] = value(cairo_edition);
}

fn add_assert_macros(document: &mut DocumentMut) -> Result<()> {
    let scarb_version = ScarbCommand::version().run()?.scarb;
    let version = if scarb_version < MINIMAL_SCARB_FOR_CORRESPONDING_ASSERT_MACROS {
        DEFAULT_ASSERT_MACROS
    } else {
        scarb_version
    };

    document
        .get_mut("dev-dependencies")
        .and_then(|dep| dep.as_table_mut())
        .context("Failed to get dev-dependencies from Scarb.toml")?
        .insert("assert_macros", value(version.to_string()));

    Ok(())
}

fn add_allow_prebuilt_macros(document: &mut DocumentMut) -> Result<()> {
    let tool_section = document.entry("tool").or_insert(Item::Table(Table::new()));
    let tool_table = tool_section
        .as_table_mut()
        .context("Failed to get tool table from Scarb.toml")?;
    tool_table.set_implicit(true);

    let mut scarb_table = Table::new();

    let mut allow_prebuilt_macros = Array::new();
    allow_prebuilt_macros.push("snforge_std");

    scarb_table.insert(
        "allow-prebuilt-plugins",
        Item::Value(Value::Array(allow_prebuilt_macros)),
    );

    tool_table.insert("scarb", Item::Table(scarb_table));

    Ok(())
}

fn extend_gitignore(path: &Path) -> Result<()> {
    if path.join(".gitignore").exists() {
        let mut file = OpenOptions::new()
            .append(true)
            .open(path.join(".gitignore"))?;
        writeln!(file, ".snfoundry_cache/")?;
        writeln!(file, "snfoundry_trace/")?;
        writeln!(file, "coverage/")?;
        writeln!(file, "profile/")?;
    }
    Ok(())
}

pub fn new(
    NewArgs {
        path,
        name,
        no_vcs,
        overwrite,
        template,
    }: NewArgs,
) -> Result<()> {
    if !overwrite {
        ensure!(
            !path.exists() || path.read_dir().is_ok_and(|mut i| i.next().is_none()),
            format!(
                "The provided path `{path}` points to a non-empty directory. If you wish to create a project in this directory, use the `--overwrite` flag"
            )
        );
    }
    let name = infer_name(name, &path)?;

    fs::create_dir_all(&path)?;
    let project_path = path.canonicalize()?;
    let scarb_manifest_path = project_path.join("Scarb.toml");
    let snfoundry_manifest_path = project_path.join("snfoundry.toml");

    // if there is no Scarb.toml run `scarb init`
    if !scarb_manifest_path.is_file() {
        let mut cmd = ScarbCommand::new_with_stdio();
        cmd.current_dir(&project_path)
            .args(["init", "--name", &name]);

        if no_vcs {
            cmd.arg("--no-vcs");
        }

        cmd.env("SCARB_INIT_TEST_RUNNER", "cairo-test")
            .run()
            .context("Failed to initialize a new project")?;

        ScarbCommand::new_with_stdio()
            .current_dir(&project_path)
            .manifest_path(scarb_manifest_path.clone())
            .offline()
            .arg("remove")
            .arg("--dev")
            .arg("cairo_test")
            .run()
            .context("Failed to remove cairo_test dependency")?;
    }

    add_template_to_scarb_manifest(&scarb_manifest_path)?;

    if !snfoundry_manifest_path.is_file() {
        create_snfoundry_manifest(&snfoundry_manifest_path)?;
    }

    add_dependencies_to_scarb_toml(&project_path, &template)?;
    update_config(&scarb_manifest_path, &template)?;
    extend_gitignore(&project_path)?;

    let template_dir = get_template_dir(&template)?;
    overwrite_or_copy_files(&template_dir, template_dir.path(), &project_path, &name)?;

    // Fetch to create lock file.
    ScarbCommand::new_with_stdio()
        .manifest_path(scarb_manifest_path)
        .arg("fetch")
        .run()
        .context("Failed to fetch created project")?;

    Ok(())
}

fn add_dependencies_to_scarb_toml(project_path: &PathBuf, template: &Template) -> Result<()> {
    let snforge_version = env!("CARGO_PKG_VERSION");
    let cairo_version = ScarbCommand::version().run()?.cairo;

    if env::var("DEV_DISABLE_SNFORGE_STD_DEPENDENCY").is_err() {
        add_dependency(project_path, "snforge_std", snforge_version, true)?;
    }

    match template {
        Template::BalanceContract => {
            add_dependency(project_path, "starknet", &cairo_version.to_string(), false)?;
        }
        Template::CairoProgram => {}
    }

    Ok(())
}

fn add_dependency(
    project_path: &PathBuf,
    dep_name: &str,
    version: &str,
    is_dev: bool,
) -> Result<()> {
    let scarb_manifest_path = project_path.join("Scarb.toml");

    let mut cmd = ScarbCommand::new_with_stdio();

    cmd.current_dir(project_path)
        .manifest_path(scarb_manifest_path.clone())
        .offline()
        .arg("add");

    if is_dev {
        cmd.arg("--dev");
    }

    cmd.arg(format!("{dep_name}@{version}"))
        .run()
        .context(format!("Failed to add {dep_name} dependency"))?;

    Ok(())
}

fn infer_name(name: Option<String>, path: &Utf8PathBuf) -> Result<String> {
    let name = if let Some(name) = name {
        name
    } else {
        let Some(file_name) = path.file_name() else {
            bail!("Cannot infer package name from path: {path}. Please: use the flag `--name`");
        };
        file_name.to_string()
    };

    Ok(name)
}

fn get_template_dir(template: &Template) -> Result<Dir> {
    let dir_name = match template {
        Template::CairoProgram => "cairo_program",
        Template::BalanceContract => "balance_contract",
    };

    TEMPLATES_DIR
        .get_dir(dir_name)
        .ok_or_else(|| anyhow!("Directory {dir_name} not found"))
        .cloned()
}
