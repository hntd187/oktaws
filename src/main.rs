#[macro_use]
extern crate failure;
#[macro_use]
extern crate serde_derive;

mod aws;
mod config;
mod okta;
mod saml;

use crate::aws::credentials::CredentialsFile;
use crate::aws::role::Role;
use crate::config::credentials;
use crate::config::organization::Organization;
use crate::config::organization::Profile;
use crate::config::organizations;
use crate::okta::auth::LoginRequest;
use crate::okta::client::Client as OktaClient;

use exitfailure::ExitFailure;
use failure::Error;
use glob::Pattern;
use log::{debug, info, trace, warn};
use rayon::iter::ParallelIterator;
use rayon::iter::IntoParallelIterator;
use rusoto_sts::Credentials;
use std::collections::HashMap;
use std::collections::HashSet;
use std::env;
use std::sync::{Arc, Mutex};
use structopt::StructOpt;

#[derive(Clone, StructOpt, Debug)]
pub struct Args {
    /// Glob of Okta profiles to update
    #[structopt(
        short = "p",
        long = "profiles",
        default_value = "*/*",
        parse(try_from_str)
    )]
    pub profiles: Pattern,

    /// Forces prompting for new credentials rather than using cache
    #[structopt(long = "force-auth")]
    pub force_auth: bool,

    /// Sets the level of verbosity
    #[structopt(short = "v", long = "verbose", parse(from_occurrences))]
    pub verbosity: usize,
}

fn main() -> Result<(), ExitFailure> {
    human_panic::setup_panic!();

    let args = Args::from_args();

    let log_level = match args.verbosity {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    env::set_var("RUST_LOG", format!("{}={}", module_path!(), log_level));
    pretty_env_logger::init();

    let credentials_store = Arc::new(Mutex::new(CredentialsFile::new(None)?));

    let mut organizations = organizations()?.peekable();

    if organizations.peek().is_none() {
        return Err(format_err!("No organizations found").into());
    }

    for organization in organizations {
        info!("Found organization {}", organization.okta_organization.name);

        let profiles = organization.profiles.clone().into_par_iter()
            .filter(|p| {
                args.profiles.matches(&format!(
                    "{}/{}",
                    organization.okta_organization.name.clone(),
                    p.name
                ))
            });

        let mut okta_client = OktaClient::new(organization.okta_organization.clone());
        let username = organization.username.to_owned();
        let password =
            credentials::get_password(&organization.okta_organization, &username, args.force_auth)?;

        let session_token = okta_client.get_session_token(&LoginRequest::from_credentials(
            username.clone(),
            password.clone(),
        ))?;

        let session_id = okta_client.new_session(session_token, &HashSet::new())?.id;
        okta_client.set_session_id(session_id.clone());

        let org_credentials: HashMap<_, _> =
            profiles
                .try_fold_with(HashMap::new(), |mut acc: HashMap<String, Credentials>,
                                          profile: Profile|
                 -> Result<HashMap<String, Credentials>, Error> {
                    let credentials = fetch_credentials(&okta_client, &organization, &profile)?;
                    acc.insert(profile.name.clone(), credentials);

                    Ok(acc)
                })
                .try_reduce_with(|mut a, b| -> Result<_, Error> {
                    a.extend(b.into_iter());
                    Ok(a)
                })
                .unwrap_or_else(|| {
                    warn!("No profiles");
                    Ok(HashMap::new())
                })?;

        for (name, creds) in org_credentials {
            credentials_store.lock().unwrap().set_profile_sts(
                format!(
                    "{}/{}",
                    organization.okta_organization.name.clone(),
                    name.clone()
                ),
                creds,
            )?;
        }

        credentials::save_credentials(&organization.okta_organization, &username, &password)?;
    }

    Arc::try_unwrap(credentials_store)
        .map_err(|_| format_err!("Failed to un-reference-count the credentials store"))?
        .into_inner()
        .map_err(|_| format_err!("Failed to un-mutex the credentials store"))?
        .save()
        .map_err(|e| e.into())
}

fn fetch_credentials(
    client: &OktaClient,
    organization: &Organization,
    profile: &Profile,
) -> Result<Credentials, Error> {
    info!(
        "Requesting tokens for {}/{}",
        &organization.okta_organization.name, profile.name
    );

    let app_link = client
        .app_links(None)?
        .into_iter()
        .find(|app_link| {
            app_link.app_name == "amazon_aws" && app_link.label == profile.application_name
        })
        .ok_or_else(|| {
            format_err!(
                "Could not find Okta application for profile {}/{}",
                organization.okta_organization.name,
                profile.name
            )
        })?;

    debug!("Application Link: {:?}", &app_link);

    let saml = client
        .get_saml_response(app_link.link_url.clone())
        .map_err(|e| {
            format_err!(
                "Error getting SAML response for profile {} ({})",
                profile.name,
                e
            )
        })?;

    trace!("SAML response: {:?}", saml);

    let roles = saml.roles;

    debug!("SAML Roles: {:?}", &roles);

    let role: Role = roles
        .into_iter()
        .find(|r| r.role_name().map(|r| r == profile.role).unwrap_or(false))
        .ok_or_else(|| {
            format_err!(
                "No matching role ({}) found for profile {}",
                profile.role,
                &profile.name
            )
        })?;

    trace!(
        "Found role: {} for profile {}",
        role.role_arn,
        &profile.name
    );

    let assumption_response = aws::role::assume_role(role, saml.raw)
        .map_err(|e| format_err!("Error assuming role for profile {} ({})", profile.name, e))?;

    let credentials = assumption_response
        .credentials
        .ok_or_else(|| format_err!("Error fetching credentials from assumed AWS role"))?;

    trace!("Credentials: {:?}", credentials);

    Ok(credentials)
}
