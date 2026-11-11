use super::prompt::{confirm, interactive};
use super::session::Session;
use super::{Failure, UninstallArgs, host_ids, print_restart_notice, raw_hosts};

pub fn run(session: &Session, args: &UninstallArgs) -> Result<u8, Failure> {
    let loaded = session.load()?;

    loaded.note_unresolved();
    let host = raw_hosts(loaded.text.as_str())
        .into_iter()
        .find(|host| host.get("id").and_then(|id| id.as_str()) == Some(args.host_id.as_str()))
        .ok_or_else(|| {
            let known = host_ids(loaded.text.as_str());

            Failure::new(if known.is_empty() {
                format!(
                    "no plugin host `{}` is configured (the configuration has no pluginHosts)",
                    args.host_id
                )
            } else {
                format!(
                    "no plugin host `{}` is configured. Configured hosts: {}",
                    args.host_id,
                    known.join(", ")
                )
            })
        })?;
    let operations: Vec<String> = host
        .get("operations")
        .and_then(|operations| operations.as_array())
        .map(|operations| {
            operations
                .iter()
                .map(|operation| {
                    format!(
                        "{}/{}",
                        operation
                            .get("pluginId")
                            .and_then(|value| value.as_str())
                            .unwrap_or("?"),
                        operation
                            .get("operation")
                            .and_then(|value| value.as_str())
                            .unwrap_or("?")
                    )
                })
                .collect()
        })
        .unwrap_or_default();

    println!(
        "Host `{}` ({})",
        args.host_id,
        host.get("endpoint")
            .and_then(|value| value.as_str())
            .unwrap_or("?")
    );
    println!("This removes {} allowed operation(s):", operations.len());

    for operation in &operations {
        println!("  {operation}");
    }

    println!("The remote service and the plugins inside it are not contacted or changed.");

    let edited = loaded
        .text
        .remove_host(&args.host_id)
        .map_err(|error| Failure::new(error.to_string()))?;

    if args.dry_run {
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

        if !confirm(&format!("Remove host `{}`?", args.host_id))? {
            println!("Cancelled. Nothing was written.");

            return Ok(1);
        }
    }

    session.commit(&loaded, &edited)?;
    println!(
        "Removed host `{}` from {}.",
        args.host_id,
        session.config_path.display()
    );
    print_restart_notice();

    Ok(0)
}
