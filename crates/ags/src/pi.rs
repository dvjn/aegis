//! How a pi launch reaches every provider pi has. Each granted provider is a pi provider of its
//! own ID, pi's own provider of each type goes to that type's default, and every other pi provider
//! is off. An extension pi loads registers them, and gives each request its provider's credential.
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
};

use aegis_wire::provider_type::{BEDROCK, VERTEX};
use anyhow::{Result, bail};
use serde::Serialize;

use crate::{
    Cli, Credentials, Need,
    clis::{SECRET_ENV, Setup, Upstream, aws_region, vertex_location},
    credentials::pi_provider,
    launch::{Offer, PiPlan, TypeDefault},
    proxy::{Route, To},
    store::write_private,
};

const EXTENSION_NAME: &str = "pi-extension.js";
const EXTENSION: &str = include_str!("pi_extension.js");
/// Carries the launch's routes to the extension.
pub(crate) const ROUTES_ENV: &str = "AGS_PI_ROUTES";
/// Where the proxy refuses requests; no provider ID starts with a dot.
const OFF: &str = "/.off";

/// One pi provider during a launch, as the extension reads it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct PiRoute {
    /// The pi provider's ID.
    id: String,
    /// The Aegis provider it reaches; `None` when there is none to reach.
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    /// pi's own provider whose models and API it uses.
    builtin: &'static str,
    /// The path after the proxy's address until the launch knows that address, then the URL.
    base_url: String,
    #[serde(flatten)]
    reach: Option<Reach>,
    /// Why every request fails.
    #[serde(skip_serializing_if = "Option::is_none")]
    refusal: Option<String>,
}

/// How pi reaches one granted provider through the proxy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Reach {
    /// The file of the provider's credential.
    credential: PathBuf,
    /// The credential is a pi sign-in, kept under the name of pi's own provider.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    sign_in: bool,
    /// The credential file holds the API key itself.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    key: bool,
    /// What pi's SDK for the provider reads, for this provider's requests only.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<&'static str, String>,
    /// What to say while the credential is missing, and once it is in place.
    missing: String,
    again: String,
}

/// The pi providers of one launch.
pub(crate) struct Launch {
    routes: Vec<PiRoute>,
    notes: Vec<String>,
}

impl Launch {
    /// Settles where each pi provider of `plan` goes, and the proxy routes that serve them.
    /// `inherited` reads the environment ags was started with.
    pub(crate) fn new(
        plan: &PiPlan,
        credentials: &Credentials,
        inherited: impl Fn(&str) -> Option<String>,
    ) -> Result<(Self, Vec<Route>)> {
        let inherited = |name: &str| inherited(name).filter(|value| !value.is_empty());
        let mut notes = vec![];
        let mut proxy = vec![];
        let mut reached = BTreeMap::new();
        for offer in &plan.offers {
            let reach = reach(offer, credentials, inherited, &mut notes);
            if reach.is_ok() {
                proxy.push(Route::provider(
                    format!("/{}", offer.id),
                    &offer.id,
                    Cli::Pi.base_path(&offer.provider_type),
                ));
            }
            reached.insert(offer.id.as_str(), reach);
        }
        if let Some(requested) = &plan.requested
            && let Some(Err(why)) = reached.get(requested.id.as_str())
        {
            bail!("{why}");
        }
        let mut refusals = vec![];
        let route = |id: &str, offer: &Offer, builtin: &'static str, refusals: &mut Vec<Route>| {
            match reached[offer.id.as_str()].clone() {
                Ok((base_url, reach)) => PiRoute {
                    id: id.to_owned(),
                    provider: Some(offer.id.clone()),
                    builtin,
                    base_url,
                    reach: Some(reach),
                    refusal: None,
                },
                Err(why) => refused(id, Some(offer), builtin, why, refusals),
            }
        };
        let mut routes = vec![];
        let mut builtins = vec![];
        for (provider_type, default) in &plan.types {
            let Some(builtin) = pi_provider(provider_type) else {
                continue;
            };
            builtins.push((builtin, default));
            let why = match default {
                TypeDefault::Use(offer) => {
                    routes.push(route(builtin, offer, builtin, &mut refusals));
                    continue;
                }
                TypeDefault::Ask(_) => format!(
                    "several {provider_type} providers can serve pi's {builtin}; choose one with `ags use pi <provider>`"
                ),
                TypeDefault::None => format!(
                    "this machine has no {provider_type} provider for pi's {builtin}; run `ags logout`, then `ags login {}` to grant one",
                    plan.server
                ),
            };
            routes.push(refused(builtin, None, builtin, why, &mut refusals));
        }
        for offer in &plan.offers {
            let Some(builtin) = pi_provider(&offer.provider_type) else {
                continue;
            };
            match builtins.iter().find(|(id, _)| *id == offer.id) {
                Some((_, TypeDefault::Use(default))) if default.id == offer.id => {}
                Some(_) => notes.push(format!(
                    "pi has its own {} provider, so pi does not offer the Aegis provider of that name",
                    offer.id
                )),
                None => routes.push(route(&offer.id, offer, builtin, &mut refusals)),
            }
        }
        proxy.extend(refusals);
        proxy.push(Route {
            prefix: OFF.to_owned(),
            to: To::Refusal("pi's providers outside Aegis are off during `ags pi`".to_owned()),
        });
        Ok((Self { routes, notes }, proxy))
    }

    /// Writes the extension into `dir` and gives pi the routes at the proxy's `origin`.
    pub(crate) fn setup(&self, origin: &str, secret: &str, dir: &Path) -> Result<Setup> {
        let extension = dir.join(EXTENSION_NAME);
        if std::fs::read(&extension).ok().as_deref() != Some(EXTENSION.as_bytes()) {
            write_private(dir, EXTENSION_NAME, EXTENSION.as_bytes())?;
        }
        let routes: Vec<PiRoute> = self
            .routes
            .iter()
            .map(|route| PiRoute {
                base_url: format!("{origin}{}", route.base_url),
                ..route.clone()
            })
            .collect();
        let launch = serde_json::json!({
            "routes": routes,
            "off": {
                "baseUrl": format!("{origin}{OFF}"),
                "refusal": "ags: pi's {provider} provider does not go through Aegis, so it is off during `ags pi`",
            },
        });
        Ok(Setup {
            args: vec!["--extension".into(), OsString::from(extension)],
            env: vec![
                (ROUTES_ENV, serde_json::to_string(&launch)?),
                (SECRET_ENV, secret.to_owned()),
            ],
            notes: self.notes.clone(),
        })
    }
}

/// A pi provider that fails with `why`, and the proxy route that refuses its requests too.
fn refused(
    id: &str,
    provider: Option<&Offer>,
    builtin: &'static str,
    why: String,
    refusals: &mut Vec<Route>,
) -> PiRoute {
    let prefix = format!("{OFF}/{}", refusals.len() + 1);
    refusals.push(Route {
        prefix: prefix.clone(),
        to: To::Refusal(why.clone()),
    });
    PiRoute {
        id: id.to_owned(),
        provider: provider.map(|offer| offer.id.clone()),
        builtin,
        base_url: prefix,
        reach: None,
        refusal: Some(format!("ags: {why}")),
    }
}

/// The path of provider `offer` after the proxy's address and how pi reaches it, or why it can't.
fn reach(
    offer: &Offer,
    credentials: &Credentials,
    inherited: impl Fn(&str) -> Option<String>,
    notes: &mut Vec<String>,
) -> Result<(String, Reach), String> {
    let (id, provider_type) = (offer.id.as_str(), offer.provider_type.as_str());
    let failed = |error: anyhow::Error| format!("{error:#}");
    let credential = credentials
        .pi_credential(id, provider_type)
        .map_err(failed)?;
    let need = Need::of_launch(Cli::Pi, provider_type)
        .ok_or_else(|| format!("pi can't run on {id}, a {provider_type} provider"))?;
    let missing = format!("ags: {id} has no {need}; run `ags setup {id}`");
    let again =
        format!("ags: {id} has its {need} from ags; run `ags setup {id}` to set it up again");
    let base = format!("/{id}{}", Cli::Pi.base_path(provider_type));
    let mut upstream = Upstream {
        id,
        provider_type,
        region: offer.region.as_deref(),
        project: None,
    };
    Ok(match provider_type {
        BEDROCK => {
            let region = aws_region(Cli::Pi, upstream, inherited).map_err(failed)?;
            let reach = Reach {
                credential,
                sign_in: false,
                key: true,
                env: BTreeMap::from([("AWS_REGION", region)]),
                missing,
                again,
            };
            (base, reach)
        }
        VERTEX => {
            let project = credentials.vertex_project(id).map_err(failed)?;
            upstream.project = Some(&project);
            let (project, location) =
                vertex_location(upstream, "GOOGLE_CLOUD_LOCATION", inherited, notes)
                    .map_err(failed)?;
            let Some(path) = credential.to_str() else {
                return Err(format!(
                    "{} is not a path pi can be given",
                    credential.display()
                ));
            };
            let env = BTreeMap::from([
                ("GOOGLE_APPLICATION_CREDENTIALS", path.to_owned()),
                ("GOOGLE_CLOUD_PROJECT", project.to_owned()),
                ("GOOGLE_CLOUD_LOCATION", location.clone()),
            ]);
            // pi leaves the project and location out of its paths once its base URL moves.
            let base = format!("{base}/v1/projects/{project}/locations/{location}");
            let reach = Reach {
                credential,
                sign_in: false,
                key: false,
                env,
                missing,
                again,
            };
            (base, reach)
        }
        _ => {
            let reach = Reach {
                credential,
                sign_in: true,
                key: false,
                env: BTreeMap::new(),
                missing,
                again,
            };
            (base, reach)
        }
    })
}

#[cfg(test)]
mod tests {
    use aegis_wire::provider_type::{CLAUDE_SUBSCRIPTION, CODEX_SUBSCRIPTION};
    use serde_json::{Value, json};

    use super::*;
    use crate::{Home, Store, proxy::SECRET_HEADER};

    const SERVER: &str = "https://aegis.example";
    const ORIGIN: &str = "http://127.0.0.1:4000";
    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    struct Fixture {
        dir: PathBuf,
        credentials: Credentials,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = crate::store::tests::scratch();
            let credentials = Credentials::new(&Store::at(dir.join("ags")), Home::at(&dir));
            Self { dir, credentials }
        }

        /// A Vertex service account for `provider` in project `proj`.
        fn vertex_account(&self, provider: &str) {
            let file = self.dir.join("account.json");
            let account = json!({
                "type": "service_account",
                "project_id": "proj",
                "client_email": "ags@proj.iam.gserviceaccount.com",
                "private_key": "unused",
            });
            std::fs::write(&file, account.to_string()).unwrap();
            self.credentials
                .save_vertex_account(provider, &file)
                .unwrap();
        }

        fn provider(&self, id: &str) -> PathBuf {
            self.dir.join("ags/providers").join(id)
        }

        fn launch(&self, plan: &PiPlan) -> (Setup, Vec<Route>) {
            let (launch, proxy) = Launch::new(plan, &self.credentials, |_| None).unwrap();
            (launch.setup(ORIGIN, SECRET, &self.dir).unwrap(), proxy)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn offer(id: &str, provider_type: &str, region: Option<&str>) -> Offer {
        Offer {
            id: id.into(),
            provider_type: provider_type.into(),
            region: region.map(str::to_owned),
        }
    }

    /// pi's types in order, each with `types`' default.
    fn plan(types: [TypeDefault; 4], offers: Vec<Offer>, requested: Option<Offer>) -> PiPlan {
        let types = Cli::Pi
            .provider_types()
            .iter()
            .map(|provider_type| (*provider_type).to_owned())
            .zip(types)
            .collect();
        PiPlan::of(SERVER.into(), offers, types, requested)
    }

    fn only(provider_type: usize, default: TypeDefault) -> [TypeDefault; 4] {
        let mut types = [const { TypeDefault::None }; 4];
        types[provider_type] = default;
        types
    }

    /// What the extension reads.
    fn extension_input(setup: &Setup) -> Value {
        let (_, input) = setup
            .env
            .iter()
            .find(|(name, _)| *name == ROUTES_ENV)
            .unwrap();
        serde_json::from_str(input).unwrap()
    }

    /// The routes pi gets, by pi provider ID.
    fn routes(setup: &Setup) -> BTreeMap<String, Value> {
        extension_input(setup)["routes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|route| (route["id"].as_str().unwrap().to_owned(), route.clone()))
            .collect()
    }

    fn prefixes(routes: &[Route]) -> Vec<(String, String)> {
        routes
            .iter()
            .map(|route| {
                let to = match &route.to {
                    To::Provider { id, base_path } => format!("{id}{base_path}"),
                    To::Refusal(why) => format!("refuse: {why}"),
                };
                (route.prefix.clone(), to)
            })
            .collect()
    }

    #[test]
    fn every_granted_provider_is_its_own_pi_provider_and_each_type_has_its_default() {
        let fixture = Fixture::new();
        fixture.vertex_account("gcp");
        let (work, chat) = (
            offer("work", CLAUDE_SUBSCRIPTION, None),
            offer("chat", CODEX_SUBSCRIPTION, None),
        );
        let (aws, aws2) = (
            offer("aws", BEDROCK, Some("eu-west-1")),
            offer("aws2", BEDROCK, None),
        );
        let gcp = offer("gcp", VERTEX, None);
        let plan = plan(
            [
                TypeDefault::Use(work.clone()),
                TypeDefault::Use(chat.clone()),
                TypeDefault::Ask(vec![aws.clone(), aws2.clone()]),
                TypeDefault::Use(gcp.clone()),
            ],
            vec![work, chat, aws, aws2, gcp],
            None,
        );
        let (setup, proxy) = fixture.launch(&plan);
        assert_eq!(
            prefixes(&proxy),
            [
                ("/work", "work"),
                ("/chat", "chat/codex"),
                ("/aws", "aws"),
                ("/gcp", "gcp"),
                (
                    "/.off/1",
                    "refuse: several bedrock providers can serve pi's amazon-bedrock; choose one with `ags use pi <provider>`"
                ),
                (
                    "/.off/2",
                    "refuse: pi needs the AWS region of aws2, which the server does not name; set AWS_REGION"
                ),
                (
                    "/.off",
                    "refuse: pi's providers outside Aegis are off during `ags pi`"
                ),
            ]
            .map(|(prefix, to)| (prefix.to_owned(), to.to_owned()))
        );
        let routes = routes(&setup);
        assert_eq!(
            routes.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "amazon-bedrock",
                "anthropic",
                "aws",
                "aws2",
                "chat",
                "gcp",
                "google-vertex",
                "openai-codex",
                "work"
            ]
        );
        assert_eq!(
            routes["anthropic"],
            json!({
                "id": "anthropic",
                "provider": "work",
                "builtin": "anthropic",
                "baseUrl": "http://127.0.0.1:4000/work",
                "credential": fixture.provider("work").join("pi/auth.json"),
                "signIn": true,
                "missing": "ags: work has no pi sign-in; run `ags setup work`",
                "again": "ags: work has its pi sign-in from ags; run `ags setup work` to set it up again",
            })
        );
        assert_eq!(routes["work"]["baseUrl"], routes["anthropic"]["baseUrl"]);
        assert_eq!(routes["work"]["provider"], "work");
        assert_eq!(
            routes["chat"]["baseUrl"],
            "http://127.0.0.1:4000/chat/codex"
        );
        assert_eq!(routes["openai-codex"]["provider"], "chat");
        assert_eq!(
            routes["aws"],
            json!({
                "id": "aws",
                "provider": "aws",
                "builtin": "amazon-bedrock",
                "baseUrl": "http://127.0.0.1:4000/aws",
                "credential": fixture.provider("aws").join("bedrock-api-key"),
                "key": true,
                "env": {"AWS_REGION": "eu-west-1"},
                "missing": "ags: aws has no Bedrock API key; run `ags setup aws`",
                "again": "ags: aws has its Bedrock API key from ags; run `ags setup aws` to set it up again",
            })
        );
        assert_eq!(
            routes["amazon-bedrock"],
            json!({
                "id": "amazon-bedrock",
                "builtin": "amazon-bedrock",
                "baseUrl": "http://127.0.0.1:4000/.off/1",
                "refusal": "ags: several bedrock providers can serve pi's amazon-bedrock; choose one with `ags use pi <provider>`",
            })
        );
        assert_eq!(routes["aws2"]["baseUrl"], "http://127.0.0.1:4000/.off/2");
        let account = fixture.provider("gcp").join("vertex-service-account.json");
        for id in ["gcp", "google-vertex"] {
            assert_eq!(
                routes[id]["baseUrl"],
                "http://127.0.0.1:4000/gcp/v1/projects/proj/locations/global"
            );
            assert_eq!(
                routes[id]["env"],
                json!({
                    "GOOGLE_APPLICATION_CREDENTIALS": account,
                    "GOOGLE_CLOUD_PROJECT": "proj",
                    "GOOGLE_CLOUD_LOCATION": "global",
                })
            );
        }
        assert!(setup.notes.is_empty());
    }

    #[test]
    fn a_type_without_a_provider_is_refused_and_so_is_every_other_pi_provider() {
        let fixture = Fixture::new();
        let work = offer("work", CLAUDE_SUBSCRIPTION, None);
        let plan = plan(only(0, TypeDefault::Use(work.clone())), vec![work], None);
        let (setup, proxy) = fixture.launch(&plan);
        assert_eq!(proxy.len(), 5);
        assert_eq!(
            routes(&setup)["google-vertex"],
            json!({
                "id": "google-vertex",
                "builtin": "google-vertex",
                "baseUrl": "http://127.0.0.1:4000/.off/3",
                "refusal": "ags: this machine has no vertex provider for pi's google-vertex; run `ags logout`, then `ags login https://aegis.example` to grant one",
            })
        );
        assert_eq!(
            extension_input(&setup)["off"],
            json!({
                "baseUrl": "http://127.0.0.1:4000/.off",
                "refusal": "ags: pi's {provider} provider does not go through Aegis, so it is off during `ags pi`",
            })
        );
    }

    #[test]
    fn a_provider_named_for_this_launch_must_be_reachable() {
        let fixture = Fixture::new();
        let aws2 = offer("aws2", BEDROCK, None);
        let plan = plan(
            only(2, TypeDefault::Use(aws2.clone())),
            vec![aws2.clone()],
            Some(aws2),
        );
        let refused = Launch::new(&plan, &fixture.credentials, |_| None)
            .err()
            .unwrap();
        assert_eq!(
            refused.to_string(),
            "pi needs the AWS region of aws2, which the server does not name; set AWS_REGION"
        );
        let (launch, _) = Launch::new(&plan, &fixture.credentials, |name| {
            (name == "AWS_REGION").then(|| "us-east-2".to_owned())
        })
        .unwrap();
        let setup = launch.setup(ORIGIN, SECRET, &fixture.dir).unwrap();
        assert_eq!(
            routes(&setup)["amazon-bedrock"]["env"],
            json!({"AWS_REGION": "us-east-2"})
        );
    }

    #[test]
    fn a_vertex_provider_without_its_account_is_refused_with_how_to_set_it_up() {
        let fixture = Fixture::new();
        let gcp = offer("gcp", VERTEX, Some("europe-west4"));
        let plan = plan(only(3, TypeDefault::Use(gcp.clone())), vec![gcp], None);
        let (setup, _) = fixture.launch(&plan);
        assert_eq!(
            routes(&setup)["gcp"]["refusal"],
            "ags: gcp has no Vertex service account; run `ags setup gcp`"
        );
        fixture.vertex_account("gcp");
        let (launch, _) = Launch::new(&plan, &fixture.credentials, |name| {
            (name == "GOOGLE_CLOUD_LOCATION").then(|| "global".to_owned())
        })
        .unwrap();
        let setup = launch.setup(ORIGIN, SECRET, &fixture.dir).unwrap();
        assert_eq!(
            routes(&setup)["gcp"]["baseUrl"],
            "http://127.0.0.1:4000/gcp/v1/projects/proj/locations/europe-west4"
        );
        assert_eq!(
            setup.notes,
            [
                "gcp serves only the europe-west4 location, so GOOGLE_CLOUD_LOCATION=global is ignored"
            ]
        );
    }

    #[test]
    fn a_provider_named_like_one_of_pis_own_is_offered_only_as_that_one() {
        let fixture = Fixture::new();
        let (anthropic, work) = (
            offer("anthropic", CLAUDE_SUBSCRIPTION, None),
            offer("work", CLAUDE_SUBSCRIPTION, None),
        );
        let offers = vec![anthropic.clone(), work.clone()];
        let (setup, _) = fixture.launch(&plan(
            only(0, TypeDefault::Use(anthropic)),
            offers.clone(),
            None,
        ));
        assert_eq!(routes(&setup)["anthropic"]["provider"], "anthropic");
        assert!(setup.notes.is_empty());
        let (setup, _) = fixture.launch(&plan(only(0, TypeDefault::Use(work)), offers, None));
        assert_eq!(routes(&setup)["anthropic"]["provider"], "work");
        assert_eq!(
            setup.notes,
            [
                "pi has its own anthropic provider, so pi does not offer the Aegis provider of that name"
            ]
        );
    }

    #[test]
    fn pi_loads_the_extension_and_gets_the_secret_only_from_its_environment() {
        let fixture = Fixture::new();
        let (setup, _) = fixture.launch(&plan(only(0, TypeDefault::None), vec![], None));
        let extension = fixture.dir.join(EXTENSION_NAME);
        assert_eq!(
            setup.args,
            [OsString::from("--extension"), extension.clone().into()]
        );
        assert!(
            setup
                .args
                .iter()
                .all(|arg| !arg.to_string_lossy().contains(SECRET))
        );
        assert!(setup.env.contains(&(SECRET_ENV, SECRET.to_owned())));
        let source = std::fs::read_to_string(&extension).unwrap();
        assert_eq!(source, EXTENSION);
        for name in [SECRET_HEADER, SECRET_ENV, ROUTES_ENV] {
            assert!(source.contains(name), "the extension uses {name}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // A rewrite renames a new file into place, which changes the inode.
            let inode = || std::fs::metadata(&extension).unwrap().ino();
            let written = inode();
            fixture.launch(&plan(only(0, TypeDefault::None), vec![], None));
            assert_eq!(inode(), written, "an unchanged extension is not rewritten");
        }
    }
}
