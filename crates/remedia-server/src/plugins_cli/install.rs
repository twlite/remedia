use remedia_server_config::edit::Json;

use super::discovery::{candidate, discover, explain};
use super::prompt::{choose, confirm, interactive, table};
use super::session::Session;
use super::{
    Failure, InstallArgs, operation_entry, parse_operation, print_restart_notice, raw_hosts,
};

pub async fn run(session: &Session, args: &InstallArgs) -> Result<u8, Failure> {
    let loaded = session.load()?;

    loaded.note_unresolved();
    let config = candidate(
        session,
        &args.url,
        args.token_env.as_deref(),
        args.allow_insecure_http,
        args.allow_private_network,
    )?;
    let origin = config.endpoint.origin().ascii_serialization();

    // An explicit ID must be valid before anything is contacted.
    if let Some(id) = &args.id
        && !remedia_server_config::valid_identifier(id)
    {
        return Err(Failure::new(
            "--id must be 1-128 letters, digits, dots, underscores or hyphens and start alphanumeric",
        ));
    }

    for value in args.operations.iter().chain(&args.replay_safe) {
        parse_operation(value)?;
    }

    println!("Discovering {origin}/.well-known/remedia ...");

    let manifest = discover(&config, None).await.map_err(|error| {
        Failure::new(format!(
            "discovery of {origin} failed: {}",
            explain(&error, args.token_env.is_some())
        ))
    })?;

    // The manifest's identity, when declared, is the host ID.
    let id = match (manifest.host_id(), args.id.as_deref()) {
        (Some(declared), Some(requested)) if declared != requested => {
            return Err(Failure::new(format!(
                "the host declares hostId `{declared}`, so the configured ID must match it (--id {requested} conflicts)"
            )));
        }
        (Some(declared), _) => declared.to_owned(),
        (None, Some(requested)) => requested.to_owned(),
        (None, None) => derive_id(manifest.name(), &config.endpoint),
    };

    if !remedia_server_config::valid_identifier(&id) {
        return Err(Failure::new(format!(
            "`{id}` is not a valid host ID. Choose one with --id"
        )));
    }

    // Conflicts: same ID, or same endpoint under another ID.
    for existing in raw_hosts(loaded.text.as_str()) {
        let existing_id = existing
            .get("id")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        let same_endpoint = loaded
            .settings
            .plugin_hosts
            .iter()
            .find(|host| host.id == existing_id)
            .is_some_and(|host| host.endpoint.origin() == config.endpoint.origin());

        if existing_id == id {
            return Err(Failure::new(if same_endpoint {
                format!(
                    "host `{id}` is already registered for {origin}. Use `plugins sync {id}` to review its operations"
                )
            } else {
                format!(
                    "a different host is already registered with ID `{id}`. Choose another with --id, or uninstall it first"
                )
            }));
        }

        if same_endpoint {
            return Err(Failure::new(format!(
                "{origin} is already registered as `{existing_id}`"
            )));
        }
    }

    let advertised = manifest.operations();

    println!();
    println!("Host:      {}", manifest.name().unwrap_or("(unnamed)"));
    println!(
        "ID:        {id}{}",
        if manifest.host_id().is_some() {
            " (declared by the host)"
        } else {
            ""
        }
    );
    println!("Endpoint:  {origin}");
    println!("Operations advertised: {}", advertised.len());

    if advertised.is_empty() {
        return Err(Failure::new(
            "the host advertises no operations, so there is nothing to allow",
        ));
    }

    let rows: Vec<Vec<String>> = advertised
        .iter()
        .enumerate()
        .map(|(index, (plugin, operation))| {
            vec![(index + 1).to_string(), plugin.clone(), operation.clone()]
        })
        .collect();

    table(&["#", "PLUGIN", "OPERATION"], &rows);
    println!();

    let selected = select_operations(args, &advertised)?;

    if selected.is_empty() {
        return Err(Failure::new(
            "no operations were selected; nothing was installed",
        ));
    }

    let mut replay_safe = Vec::new();

    for value in &args.replay_safe {
        let pair = parse_operation(value)?;

        if !selected.contains(&pair) {
            return Err(Failure::new(format!(
                "--replay-safe {value} is not one of the selected operations"
            )));
        }

        replay_safe.push(pair);
    }

    let mut members = vec![
        ("id".to_owned(), Json::str(&id)),
        ("endpoint".to_owned(), Json::str(&origin)),
    ];

    if let Some(variable) = &args.token_env {
        members.push((
            "auth".to_owned(),
            Json::object([
                ("type", Json::str("bearer")),
                ("token", Json::str(format!("env('{variable}')"))),
            ]),
        ));
    }

    if args.allow_insecure_http {
        members.push(("allowInsecureHttp".to_owned(), Json::Bool(true)));
    }

    if args.allow_private_network {
        members.push(("allowPrivateNetwork".to_owned(), Json::Bool(true)));
    }

    members.push((
        "operations".to_owned(),
        Json::Array(
            selected
                .iter()
                .map(|(plugin, operation)| {
                    operation_entry(
                        plugin,
                        operation,
                        replay_safe.contains(&(plugin.clone(), operation.clone())),
                    )
                })
                .collect(),
        ),
    ));

    let edited = loaded
        .text
        .add_host(&Json::Object(members))
        .map_err(|error| Failure::new(error.to_string()))?;

    println!("Allowed operations ({}):", selected.len());

    for (plugin, operation) in &selected {
        let marker = if replay_safe.contains(&(plugin.clone(), operation.clone())) {
            "  (replay-safe)"
        } else {
            ""
        };

        println!("  {plugin}/{operation}{marker}");
    }

    println!("Replay-safe retries stay off unless you pass --replay-safe.");

    if args.dry_run {
        // Validate exactly what would be written, so a dry run predicts the real outcome.
        session.validate(edited.as_str()).map_err(|error| {
            Failure::new(format!(
                "the resulting configuration would be invalid: {}",
                super::session::describe(&error)
            ))
        })?;
        println!(
            "\nDry run: {} was not modified.",
            session.config_path.display()
        );

        return Ok(0);
    }

    if !args.yes {
        if !interactive() {
            return Err(Failure::new(
                "confirmation is required: pass --yes to confirm in a noninteractive session",
            ));
        }

        if !confirm(&format!(
            "Add host `{id}` to {}?",
            session.config_path.display()
        ))? {
            println!("Cancelled. Nothing was written.");

            return Ok(1);
        }
    }

    session.commit(&loaded, &edited)?;
    println!("Added host `{id}` to {}.", session.config_path.display());

    if let Some(variable) = &args.token_env {
        println!(
            "The bearer token is read from ${variable} at startup; it is not stored in the file."
        );
    }

    print_restart_notice();

    Ok(0)
}

/// Choose operations from flags, or interactively. A noninteractive session must be explicit.
fn select_operations(
    args: &InstallArgs,
    advertised: &[(String, String)],
) -> Result<Vec<(String, String)>, Failure> {
    if args.all_operations && !args.operations.is_empty() {
        return Err(Failure::new(
            "use either --all-operations or --operation, not both",
        ));
    }

    if args.all_operations {
        return Ok(advertised.to_vec());
    }

    if !args.operations.is_empty() {
        let mut selected = Vec::new();

        for value in &args.operations {
            let pair = parse_operation(value)?;

            if !advertised.contains(&pair) {
                return Err(Failure::new(format!(
                    "the host does not advertise {value}. Allowing it would not make it callable"
                )));
            }

            if !selected.contains(&pair) {
                selected.push(pair);
            }
        }

        return Ok(selected);
    }

    if !interactive() {
        return Err(Failure::new(
            "choose operations explicitly: pass --operation PLUGIN/OPERATION (repeatable) or --all-operations",
        ));
    }

    let chosen = choose(
        "Allow which operations? Numbers separated by commas, `all`, or nothing to cancel:",
        advertised.len(),
    )?;

    Ok(chosen
        .into_iter()
        .map(|index| advertised[index].clone())
        .collect())
}

/// A valid ID from the display name, falling back to the hostname.
fn derive_id(name: Option<&str>, endpoint: &url::Url) -> String {
    let slug = |text: &str| {
        let mut slug = String::new();

        for character in text.chars() {
            if character.is_ascii_alphanumeric() {
                slug.push(character.to_ascii_lowercase());
            } else if !slug.ends_with('-') && !slug.is_empty() {
                slug.push('-');
            }
        }

        slug.trim_matches('-').chars().take(64).collect::<String>()
    };
    let from_name = name.map(slug).unwrap_or_default();

    if !from_name.is_empty() {
        return from_name;
    }

    let host = slug(endpoint.host_str().unwrap_or("plugin-host"));

    if host.is_empty() {
        "plugin-host".to_owned()
    } else {
        host
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_derived_from_names_then_hostnames() {
        let url = url::Url::parse("https://plugins.example.com").unwrap();

        assert_eq!(
            derive_id(Some("YouTube Music (EU)!"), &url),
            "youtube-music-eu"
        );
        assert_eq!(derive_id(Some("***"), &url), "plugins-example-com");
        assert_eq!(derive_id(None, &url), "plugins-example-com");
        assert!(remedia_server_config::valid_identifier(&derive_id(
            Some("日本語"),
            &url
        )));
    }
}
