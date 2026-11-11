use super::session::Session;
use super::{Failure, ToggleArgs, host_ids, print_restart_notice, raw_hosts};

/// Enable or disable a host. The entry keeps its endpoint, credentials, operations and limits.
pub fn run(session: &Session, args: &ToggleArgs, enable: bool) -> Result<u8, Failure> {
    let loaded = session.load()?;

    loaded.note_unresolved();
    let host = raw_hosts(loaded.text.as_str())
        .into_iter()
        .find(|host| host.get("id").and_then(|id| id.as_str()) == Some(args.host_id.as_str()))
        .ok_or_else(|| {
            Failure::new(format!(
                "no plugin host `{}` is configured. Configured hosts: {}",
                args.host_id,
                host_ids(loaded.text.as_str()).join(", ")
            ))
        })?;
    let enabled = host
        .get("enabled")
        .and_then(|value| value.as_bool())
        .unwrap_or(true);
    let word = if enable { "enabled" } else { "disabled" };

    if enabled == enable {
        println!("Host `{}` is already {word}. Nothing to do.", args.host_id);

        return Ok(0);
    }

    let edited = loaded
        .text
        .set_host_enabled(&args.host_id, enable)
        .map_err(|error| Failure::new(error.to_string()))?;

    if args.dry_run {
        session.validate(edited.as_str()).map_err(|error| {
            Failure::new(format!(
                "the resulting configuration would be invalid: {}",
                super::session::describe(&error)
            ))
        })?;
        println!(
            "Dry run: host `{}` would be {word}. {} was not modified.",
            args.host_id,
            session.config_path.display()
        );

        return Ok(0);
    }

    session.commit(&loaded, &edited)?;
    println!("Host `{}` is now {word}.", args.host_id);

    if !enable {
        println!(
            "Its endpoint, credentials, operations and limits are kept; run `plugins enable {}` to restore it.",
            args.host_id
        );
    }

    print_restart_notice();

    Ok(0)
}
