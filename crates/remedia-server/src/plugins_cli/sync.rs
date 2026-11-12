//! `plugins sync`: compare configured allowlists with live manifests.
//!
//! Read-only by default. Applying changes needs explicit approval, never enables a new
//! operation implicitly, never changes `replaySafe` values, host identities or credentials,
//! and never touches a host which could not be reached.
use serde_json::Value;

use super::discovery::explain;
use super::inspect::{Check, check_host, compare, configured_operations, failure_kind};
use super::prompt::{confirm, interactive};
use super::session::Session;
use super::{
    Failure, SyncArgs, host_ids, operation_entry, parse_operation, print_restart_notice, raw_hosts,
};

struct Pending {
    host_id: String,
    remove: Vec<(String, String)>,
    add: Vec<(String, String)>,
}

pub async fn run(session: &Session, args: &SyncArgs) -> Result<u8, Failure> {
    let loaded = session.load()?;

    loaded.note_unresolved();
    let mut hosts = raw_hosts(loaded.text.as_str());

    if let Some(selected) = &args.host_id {
        hosts.retain(|host| host.get("id").and_then(Value::as_str) == Some(selected.as_str()));

        if hosts.is_empty() {
            return Err(Failure::new(format!(
                "no plugin host `{selected}` is configured. Configured hosts: {}",
                host_ids(loaded.text.as_str()).join(", ")
            )));
        }
    }

    if hosts.is_empty() {
        println!("No plugin hosts are configured. Nothing to synchronize.");

        return Ok(0);
    }

    let requested_additions = args
        .add
        .iter()
        .map(|value| parse_operation(value))
        .collect::<Result<Vec<_>, _>>()?;

    if !args.apply && (!requested_additions.is_empty() || args.remove_missing || args.yes) {
        return Err(Failure::new(
            "--add, --remove-missing and --yes only apply together with --apply",
        ));
    }

    if args.apply
        && !args.dry_run
        && !interactive()
        && (!args.yes || (requested_additions.is_empty() && !args.remove_missing))
    {
        return Err(Failure::new(
            "applying in a noninteractive session needs explicit changes: pass --add PLUGIN/OPERATION and/or --remove-missing, plus --yes",
        ));
    }

    let mut pending = Vec::new();
    let mut unresolved_problems = 0_u32;
    let mut differences = 0_usize;
    let mut unmatched_additions = requested_additions.clone();

    for entry in &hosts {
        let id = entry
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let url = entry.get("url").and_then(Value::as_str).unwrap_or("?");
        let enabled = entry
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let label = if enabled { "" } else { " (disabled)" };

        println!("{id}  {url}{label}");

        match check_host(session, entry, &id).await {
            Check::NotChecked(reason) => {
                unresolved_problems += 1;
                println!("  not checked: {reason}. Configuration left unchanged.");
            }
            Check::Failed(error, has_token) => {
                unresolved_problems += 1;
                println!(
                    "  {}: {}. Configuration left unchanged.",
                    failure_kind(&error).replace('_', " "),
                    explain(&error, has_token)
                );
            }
            Check::Reached(manifest) => {
                let difference = compare(&configured_operations(entry), &manifest);

                if difference.missing.is_empty() && difference.new.is_empty() {
                    println!(
                        "  in sync ({} operation(s))",
                        configured_operations(entry).len()
                    );

                    continue;
                }

                for (plugin, operation) in &difference.missing {
                    println!(
                        "  - {plugin}/{operation}   allowed, but the host no longer advertises it"
                    );
                }

                for (plugin, operation) in &difference.new {
                    println!(
                        "  + {plugin}/{operation}   advertised, not allowed (stays off unless you approve it)"
                    );
                }

                differences += difference.missing.len() + difference.new.len();

                if args.apply {
                    let mut change = Pending {
                        host_id: id.clone(),
                        remove: Vec::new(),
                        add: Vec::new(),
                    };

                    for pair in &difference.missing {
                        let approved = args.remove_missing
                            || (interactive()
                                && !args.yes
                                && confirm(&format!("  Remove {}/{} from {id}?", pair.0, pair.1))?);

                        if approved {
                            change.remove.push(pair.clone());
                        }
                    }

                    for pair in &difference.new {
                        let flagged = requested_additions.contains(pair);
                        let approved = flagged
                            || (interactive()
                                && !args.yes
                                && confirm(&format!(
                                    "  Allow {}/{} on {id}? (replay-safe stays off)",
                                    pair.0, pair.1
                                ))?);

                        if flagged {
                            unmatched_additions.retain(|candidate| candidate != pair);
                        }

                        if approved {
                            change.add.push(pair.clone());
                        }
                    }

                    pending.push(change);
                }
            }
        }
    }

    if args.apply {
        // An --add which matched nothing is a mistake worth stopping for, not a silent no-op.
        if let Some((plugin, operation)) = unmatched_additions.first() {
            return Err(Failure::new(format!(
                "--add {plugin}/{operation} is not a newly advertised operation of the selected host(s); nothing was written"
            )));
        }

        let changes: usize = pending
            .iter()
            .map(|change| change.remove.len() + change.add.len())
            .sum();

        if changes == 0 {
            println!("\nNo changes approved. Nothing was written.");

            return Ok(u8::from(unresolved_problems > 0 || differences > 0) * 2);
        }

        let mut edited = loaded
            .text
            .clone_text()
            .map_err(|error| Failure::new(error.to_string()))?;

        for change in &pending {
            let additions: Vec<_> = change
                .add
                .iter()
                .map(|(plugin, operation)| operation_entry(plugin, operation, false))
                .collect();

            edited = edited
                .change_host_operations(&change.host_id, &change.remove, &additions)
                .map_err(|error| Failure::new(error.to_string()))?;
        }

        println!("\n{changes} change(s) approved.");

        if args.dry_run {
            session.validate(edited.as_str()).map_err(|error| {
                Failure::new(format!(
                    "the resulting configuration would be invalid: {}",
                    super::session::describe(&error)
                ))
            })?;
            println!(
                "Dry run: {} was not modified.",
                session.config_path.display()
            );

            return Ok(0);
        }

        if !args.yes
            && interactive()
            && !confirm(&format!("Write them to {}?", session.config_path.display()))?
        {
            println!("Cancelled. Nothing was written.");

            return Ok(1);
        }

        session.commit(&loaded, &edited)?;
        println!("Updated {}.", session.config_path.display());
        print_restart_notice();

        return Ok(u8::from(unresolved_problems > 0) * 2);
    }

    println!();

    if differences == 0 && unresolved_problems == 0 {
        println!("Everything is in sync. Nothing was changed.");

        return Ok(0);
    }

    println!(
        "{differences} difference(s), {unresolved_problems} host(s) not checked. Nothing was changed."
    );

    if differences > 0 {
        println!(
            "Review them, then run `remedia-server plugins sync{} --apply` to approve changes.",
            args.host_id
                .as_ref()
                .map(|id| format!(" {id}"))
                .unwrap_or_default()
        );
    }

    Ok(2)
}
