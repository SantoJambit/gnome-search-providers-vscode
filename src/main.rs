// Copyright Sebastian Wiesner <sebastian@swsnr.de>
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

#![deny(warnings, missing_docs, clippy::all)]

//! Gnome search provider for VSCode editors.

use std::collections::HashMap;
use std::convert::TryInto;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use gio::{AppInfoExt, IconExt};
use log::{debug, error, info, warn};
use serde::Deserialize;
use std::borrow::Borrow;
use zbus::export::zvariant;
use zbus::fdo::RequestNameReply;
use zbus::{dbus_interface, fdo};

#[derive(Debug, Deserialize)]
struct StorageOpenedPathsList {
    /// Up to code 1.54
    workspaces3: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct Storage {
    #[serde(rename = "openedPathsList")]
    opened_paths_list: Option<StorageOpenedPathsList>,
}

/// Read a VSCode storage.json from the given `reader`.
fn read_storage<R: Read>(reader: R) -> Result<Storage> {
    serde_json::from_reader(reader).map_err(Into::into)
}

/// Read the `storage.json` file in the given `config_dir`.
fn read_storage_from_dir<P: AsRef<Path>>(config_dir: P) -> Result<Storage> {
    let path = config_dir.as_ref().join("storage.json");
    read_storage(
        File::open(&path)
            .with_context(|| format!("Failed to open {} for reading", path.display()))?,
    )
    .with_context(|| format!("Failed to parse storage from {}", path.display()))
}

#[derive(Debug, Copy, Clone)]
struct ConfigLocation<'a> {
    dirname: &'a str,
}

/// A search provider to expose from this service.
struct ProviderDefinition<'a> {
    /// A human readable label for this provider.
    label: &'a str,
    /// The ID (that is, the filename) of the desktop file of the corresponding app.
    desktop_id: &'a str,
    /// The relative object path to expose this provider at.
    relative_obj_path: &'a str,
    /// The location of the configuration for this app.
    config: ConfigLocation<'a>,
}

impl ProviderDefinition<'_> {
    /// Gets the full object path for this provider.
    fn objpath(&self) -> String {
        format!("/de/swsnr/searchprovider/vscode/{}", self.relative_obj_path)
    }
}

/// Known search providers.
///
/// For each definition in this array a corresponding provider file must exist in
/// `providers/`; the file must refer to the same `desktop_id` and the same object path.
/// The object path must be unique for each desktop ID, to ensure that this service always
/// launches the right application associated with the search provider.
const PROVIDERS: &[ProviderDefinition] = &[ProviderDefinition {
    label: "Code OSS (Arch Linux)",
    desktop_id: "code-oss.desktop",
    relative_obj_path: "arch/codeoss",
    config: ConfigLocation {
        dirname: "Code - OSS",
    },
}];

/// A recent workspace of a VSCode variant.
#[derive(Debug, PartialEq)]
struct RecentWorkspace {
    /// The human readable nfame.
    name: String,
    /// The workspace URL.
    url: String,
}

/// Find all workspaces from `workspaces` which match the given `terms`.
///
/// `workspaces` is an iterator over pairs of `(id, workspace)`.
///
/// For each `workspace` match `terms` against the name and the `url` and return
/// a vector with all `id`s of worksapces which match.
///
/// Currently this simply checks whether all terms are container in `url` or `name`
/// but this is subject to change.
fn find_matching_workspaces<'a, I, S, T, P>(
    workspaces: I,
    terms: &'a [S],
) -> impl Iterator<Item = T> + 'a
where
    I: Iterator<Item = (T, P)> + 'a,
    P: Borrow<RecentWorkspace>,
    T: AsRef<str>,
    S: AsRef<str>,
{
    workspaces.filter_map(move |(id, project)| {
        Some(id).filter(move |_| {
            terms.iter().all(|term| {
                project
                    .borrow()
                    .name
                    .to_lowercase()
                    .contains(&term.as_ref().to_lowercase())
            }) || terms.iter().all(|term| {
                project
                    .borrow()
                    .url
                    .to_lowercase()
                    .contains(&term.as_ref().to_lowercase())
            })
        })
    })
}

/// A DBus search provider for a VSCode variant.
struct VscodeSearchProvider {
    /// The app to launch for search results.
    app: gio::DesktopAppInfo,
    /// All known recents workspaces.
    recent_workspaces: HashMap<String, RecentWorkspace>,
    /// The configuration directory.
    config_dir: PathBuf,
}

impl VscodeSearchProvider {
    /// Update recent workspaces.
    ///
    /// Clears the map of recent workspaces and reads the recent workspaces from storage again.
    ///
    /// If the file fails to read return the corresponding error and leave the map of projects empty.
    fn update_recent_workspaces(&mut self) -> Result<()> {
        info!(
            "Updating recent workspaces for {}",
            self.app.get_id().unwrap()
        );
        self.recent_workspaces.clear();
        let storage = read_storage_from_dir(&self.config_dir)?;
        if let Some(workspaces3) = storage
            .opened_paths_list
            .and_then(|paths| paths.workspaces3)
        {
            for url in workspaces3 {
                if let Some(name) = url.split('/').last() {
                    let id = format!(
                        "vscode-search-provider-{}-{}",
                        self.app.get_id().unwrap(),
                        &url
                    );
                    self.recent_workspaces.insert(
                        id,
                        RecentWorkspace {
                            name: name.to_string(),
                            url,
                        },
                    );
                }
            }
        }
        info!(
            "Found {} workspace(s) for {}",
            self.recent_workspaces.len(),
            self.app.get_id().unwrap()
        );
        Ok(())
    }
}

/// The DBus interface of the search provider.
///
/// See <https://developer.gnome.org/SearchProvider/> for information.
#[dbus_interface(name = "org.gnome.Shell.SearchProvider2")]
impl VscodeSearchProvider {
    /// Starts a search.
    ///
    /// This function is called when a new search is started. It gets an array of search terms as arguments,
    /// and should return an array of result IDs. gnome-shell will call GetResultMetas for (some) of these result
    /// IDs to get details about the result that can be be displayed in the result list.
    fn get_initial_result_set(&mut self, terms: Vec<String>) -> zbus::fdo::Result<Vec<String>> {
        debug!(
            "Searching for {:?} of {}",
            terms,
            self.app.get_id().unwrap()
        );
        self.update_recent_workspaces().map_err(|error| {
            error!(
                "Failed to update recent workspaces for {} at {:?}: {}",
                self.app.get_id().unwrap(),
                self.config_dir.display(),
                error
            );
            zbus::fdo::Error::Failed(format!(
                "Failed to update recent workspaces for {}: {}",
                self.app.get_id().unwrap(),
                error
            ))
        })?;

        let ids = find_matching_workspaces(self.recent_workspaces.iter(), terms.as_slice())
            .map(String::to_owned)
            .collect();
        debug!("Found ids {:?} for {}", ids, self.app.get_id().unwrap());
        Ok(ids)
    }

    /// Refine an ongoing search.
    ///
    /// This function is called to refine the initial search results when the user types more characters in the search entry.
    /// It gets the previous search results and the current search terms as arguments, and should return an array of result IDs,
    /// just like GetInitialResulSet.
    fn get_subsearch_result_set(
        &self,
        previous_results: Vec<String>,
        terms: Vec<String>,
    ) -> Vec<String> {
        debug!(
            "Searching for {:?} in {:?} of {}",
            terms,
            previous_results,
            self.app.get_id().unwrap()
        );
        let candidates = previous_results
            .iter()
            .filter_map(|id| self.recent_workspaces.get(id).map(|p| (id, p)));

        let ids = find_matching_workspaces(candidates, terms.as_slice())
            .map(String::to_owned)
            .collect();
        debug!("Found ids {:?} for {}", ids, self.app.get_id().unwrap());
        ids
    }

    /// Get metadata for results.
    ///
    /// This function is called to obtain detailed information for results.
    /// It gets an array of result IDs as arguments, and should return a matching array of dictionaries
    /// (ie one a{sv} for each passed-in result ID).
    ///
    /// The following pieces of information should be provided for each result:
    //
    //  - "id": the result ID
    //  - "name": the display name for the result
    //  - "icon": a serialized GIcon (see g_icon_serialize()), or alternatively,
    //  - "gicon": a textual representation of a GIcon (see g_icon_to_string()), or alternativly,
    //  - "icon-data": a tuple of type (iiibiiay) describing a pixbuf with width, height, rowstride, has-alpha, bits-per-sample, and image data
    //  - "description": an optional short description (1-2 lines)
    fn get_result_metas(&self, results: Vec<String>) -> Vec<HashMap<String, zvariant::Value>> {
        debug!("Getting meta info for {:?}", results);
        results
            .into_iter()
            .filter_map(|id| {
                self.recent_workspaces.get(&id).map(|workspace| {
                    debug!("Compiling meta info for {}", id);
                    let icon = IconExt::to_string(&self.app.get_icon().unwrap()).unwrap();
                    debug!("Using icon {} for id {}", icon, id);

                    let mut meta: HashMap<String, zvariant::Value> = HashMap::new();
                    meta.insert("id".to_owned(), id.into());
                    meta.insert("name".to_owned(), (&workspace.name).into());
                    meta.insert("gicon".to_owned(), icon.to_string().into());
                    meta.insert("description".to_owned(), workspace.url.to_string().into());
                    meta
                })
            })
            .collect()
    }

    /// Activate an individual result.
    ///
    /// This function is called when the user clicks on an individual result to open it in the application.
    /// The arguments are the result ID, the current search terms and a timestamp.
    ///
    /// Launches the underlying Jetbrains app with the path to the selected project.
    fn activate_result(
        &self,
        id: String,
        terms: Vec<String>,
        timestamp: u32,
    ) -> zbus::fdo::Result<()> {
        debug!("Activating result {} for {:?} at {}", id, terms, timestamp);
        if let Some(workspace) = self.recent_workspaces.get(&id) {
            info!("Launching recent workspace {:?}", workspace);
            self.app
                .launch_uris::<gio::AppLaunchContext>(&[workspace.url.as_str()], None)
                .map_err(|error| {
                    error!(
                        "Failed to launch app {} for URL {}: {}",
                        self.app.get_id().unwrap(),
                        workspace.url,
                        error
                    );
                    zbus::fdo::Error::SpawnFailed(format!(
                        "Failed to launch app {} for URL {}: {}",
                        self.app.get_id().unwrap(),
                        workspace.url,
                        error
                    ))
                })
        } else {
            error!("Project with ID {} not found", id);
            Err(zbus::fdo::Error::Failed(format!("Result {} not found", id)))
        }
    }

    /// Launch a search within the App.
    ///
    /// This function is called when the user clicks on the provider icon to display more search results in the application.
    /// The arguments are the current search terms and a timestamp.
    ///
    /// We cannot remotely popup the project manager dialog of the underlying Jetbrains App; there's no such command line flag.
    /// Hence we simply launch the app without any arguments to bring up the start screen if it's not yet running.
    fn launch_search(&self, terms: Vec<String>, timestamp: u32) -> zbus::fdo::Result<()> {
        debug!("Launching search for {:?} at {}", terms, timestamp);
        info!("Launching app {} directly", self.app.get_id().unwrap());
        self.app
            .launch::<gio::AppLaunchContext>(&[], None)
            .map_err(|error| {
                error!(
                    "Failed to launch app {}: {}",
                    self.app.get_id().unwrap(),
                    error
                );
                zbus::fdo::Error::SpawnFailed(format!(
                    "Failed to launch app {}: {}",
                    self.app.get_id().unwrap(),
                    error
                ))
            })
    }
}

/// The name to request on the bus.
const BUSNAME: &str = "de.swsnr.searchprovider.VSCode";

/// Starts the DBUS service loop.
///
/// Register all providers whose underlying app is installed.
fn start_dbus_service_loop() -> Result<()> {
    let connection =
        zbus::Connection::new_session().with_context(|| "Failed to connect to session bus")?;

    let user_config_dir =
        dirs::config_dir().with_context(|| "No configuration directory for current user!")?;

    let mut object_server = zbus::ObjectServer::new(&connection);
    for provider in PROVIDERS {
        if let Some(app) = gio::DesktopAppInfo::new(provider.desktop_id) {
            info!(
                "Registering provider for {} at {}",
                provider.desktop_id,
                provider.objpath()
            );
            let dbus_provider = VscodeSearchProvider {
                config_dir: user_config_dir.join(provider.config.dirname),
                app,
                recent_workspaces: HashMap::new(),
            };
            object_server.at(&provider.objpath().try_into()?, dbus_provider)?;
        }
    }

    info!("All providers registered, acquiring {}", BUSNAME);
    let reply = fdo::DBusProxy::new(&connection)?
        .request_name(BUSNAME, fdo::RequestNameFlags::DoNotQueue.into())
        .with_context(|| format!("Request to acquire bus name {} failed", BUSNAME))?;
    if reply == RequestNameReply::PrimaryOwner {
        info!("Acquired name {}, handling incoming messages", BUSNAME);
        loop {
            match object_server.try_handle_next() {
                Ok(None) => debug!("Interface message processed"),
                Ok(Some(message)) => warn!("Message not handled by interfaces: {:?}", message),
                Err(err) => error!("{}", err),
            }
        }
    } else {
        Err(anyhow!(
            "Failed to acquire {} as primary owner (reply from bus: {:?})",
            BUSNAME,
            reply
        ))
    }
}

fn main() {
    use clap::*;

    let app = app_from_crate!()
        .setting(AppSettings::UnifiedHelpMessage)
        .setting(AppSettings::DontCollapseArgsInUsage)
        .setting(AppSettings::DeriveDisplayOrder)
        .set_term_width(80)
        .after_help(
            "\
Set $RUST_LOG to control the log level",
        )
        .arg(
            Arg::with_name("providers")
                .long("--providers")
                .help("List all providers"),
        );
    let matches = app.get_matches();
    if matches.is_present("providers") {
        let mut labels: Vec<&'static str> = PROVIDERS.iter().map(|p| p.label).collect();
        labels.sort_unstable();
        for label in labels {
            println!("{}", label)
        }
    } else {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

        info!(
            "Started jetbrains search provider version: {}",
            env!("CARGO_PKG_VERSION")
        );

        if let Err(err) = start_dbus_service_loop() {
            error!("Failed to start DBus event loop: {}", err);
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::read_storage;

    #[test]
    fn read_recent_workspaces_code_1_54() {
        let data: &[u8] = include_bytes!("tests/code_1_54_storage.json");
        let storage = read_storage(data).unwrap();
        assert!(
            &storage.opened_paths_list.is_some(),
            "opened paths list missing"
        );
        assert!(
            &storage
                .opened_paths_list
                .as_ref()
                .unwrap()
                .workspaces3
                .is_some(),
            "workspaces3 missing"
        );
        assert_eq!(
            storage
                .opened_paths_list
                .and_then(|p| p.workspaces3)
                .unwrap(),
            vec![
                "file:///home/foo//mdcat",
                "file:///home/foo//gnome-jetbrains-search-provider",
                "file:///home/foo//gnome-shell",
                "file:///home/foo//sbctl",
            ]
        )
    }

    mod search {
        use crate::{find_matching_workspaces, RecentWorkspace};

        fn do_match<'a>(projects: &[(&'a str, RecentWorkspace)], terms: &[&str]) -> Vec<&'a str> {
            find_matching_workspaces(projects.iter().map(|(s, p)| (*s, p)), terms).collect()
        }

        #[test]
        fn matches_something() {
            let workspaces = vec![(
                "foo",
                RecentWorkspace {
                    name: "mdcat".to_string(),
                    url: "file:///home/foo/dev/mdcat".to_string(),
                },
            )];
            assert_eq!(do_match(&workspaces, &["mdcat"]), ["foo"]);
        }

        #[test]
        fn do_not_find_undesired_projects() {
            let workspaces = vec![
                (
                    "foo-1",
                    RecentWorkspace {
                        name: "ui-pattern-library".to_string(),
                        url: "file:///home/foo/dev/something/ui-pattern-library".to_string(),
                    },
                ),
                (
                    "foo-2",
                    RecentWorkspace {
                        name: "dauntless-builder".to_string(),
                        url: "file:///home/foo/dev/dauntless-builder".to_string(),
                    },
                ),
                (
                    "foo-3",
                    RecentWorkspace {
                        name: "typo3-ssr".to_string(),
                        url: "file:///home/foo/dev/something/typo3-ssr".to_string(),
                    },
                ),
            ];
            assert!(do_match(&workspaces, &["flutter_test_app"]).is_empty());
        }

        #[test]
        fn ignore_case_of_name() {
            let workspaces = vec![(
                "foo",
                RecentWorkspace {
                    name: "mdCat".to_string(),
                    url: "file:///home/foo/dev/foo".to_string(),
                },
            )];
            assert_eq!(do_match(&workspaces, &["Mdcat"]), ["foo"]);
        }

        #[test]
        fn ignore_case_of_url() {
            let workspaces = vec![(
                "foo",
                RecentWorkspace {
                    name: "bar".to_string(),
                    url: "file:///home/foo/dev/mdcaT".to_string(),
                },
            )];
            assert_eq!(do_match(&workspaces, &["Mdcat"]), ["foo"]);
        }
    }

    mod providers {
        use crate::{BUSNAME, PROVIDERS};
        use anyhow::{Context, Result};
        use ini::Ini;
        use std::collections::HashSet;
        use std::path::Path;

        struct ProviderFile {
            desktop_id: String,
            object_path: String,
            bus_name: String,
            version: String,
        }

        fn load_all_provider_files() -> Result<Vec<ProviderFile>> {
            let mut providers = Vec::new();
            let ini_files = globwalk::GlobWalkerBuilder::new(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("providers"),
                "*.ini",
            )
            .build()
            .unwrap();
            for entry in ini_files {
                let filepath = entry.unwrap().into_path();
                let ini = Ini::load_from_file(&filepath).with_context(|| {
                    format!("Failed to parse ini file at {}", filepath.display())
                })?;
                let provider = ProviderFile {
                    desktop_id: ini
                        .get_from(Some("Shell Search Provider"), "DesktopId")
                        .with_context(|| format!("DesktopId missing in {}", &filepath.display()))?
                        .to_string(),
                    object_path: ini
                        .get_from(Some("Shell Search Provider"), "ObjectPath")
                        .with_context(|| format!("ObjectPath missing in {}", &filepath.display()))?
                        .to_string(),
                    bus_name: ini
                        .get_from(Some("Shell Search Provider"), "BusName")
                        .with_context(|| format!("BusName missing in {}", &filepath.display()))?
                        .to_string(),
                    version: ini
                        .get_from(Some("Shell Search Provider"), "Version")
                        .with_context(|| format!("Version missing in {}", &filepath.display()))?
                        .to_string(),
                };
                providers.push(provider);
            }

            Ok(providers)
        }

        #[test]
        fn all_providers_have_a_correct_ini_file() {
            let provider_files = load_all_provider_files().unwrap();
            for provider in PROVIDERS {
                let provider_file = provider_files
                    .iter()
                    .find(|p| p.desktop_id == provider.desktop_id);
                assert!(
                    provider_file.is_some(),
                    "Provider INI missing for provider {} with desktop ID {}",
                    provider.label,
                    provider.desktop_id
                );

                assert_eq!(provider_file.unwrap().object_path, provider.objpath());
                assert_eq!(provider_file.unwrap().bus_name, BUSNAME);
                assert_eq!(provider_file.unwrap().version, "2");
            }
        }

        #[test]
        fn no_extra_ini_files_without_providers() {
            let provider_files = load_all_provider_files().unwrap();
            assert_eq!(PROVIDERS.len(), provider_files.len());
        }

        #[test]
        fn desktop_ids_are_unique() {
            let mut ids = HashSet::new();
            for provider in PROVIDERS {
                ids.insert(provider.desktop_id);
            }
            assert_eq!(PROVIDERS.len(), ids.len());
        }

        #[test]
        fn dbus_paths_are_unique() {
            let mut paths = HashSet::new();
            for provider in PROVIDERS {
                paths.insert(provider.objpath());
            }
            assert_eq!(PROVIDERS.len(), paths.len());
        }
    }
}
