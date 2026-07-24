use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::api::client::{ApiClient, ApiClientError};
use crate::api::schema::{
    AgentPromptParams, AgentReadParams, AgentRenameParams, AgentSendParams, AgentStartParams,
    AgentStatus, AgentTarget, EmptyParams, EventsSubscribeParams, Method, ReadFormat, ReadSource,
    Request, Subscription, SubscriptionEventData, SubscriptionEventEnvelope,
};

/// Upstream `agent prompt --wait` requires an observed state change within this
/// window after submitting from a non-working state; otherwise it stalls.
const AGENT_PROMPT_EFFECT_TIMEOUT_MS: u64 = 5_000;

pub(super) fn run_agent_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_agent_help();
        return Ok(2);
    };

    match subcommand {
        "list" => agent_list(&args[1..]),
        "get" => agent_get(&args[1..]),
        "read" => agent_read(&args[1..]),
        "send" => agent_send(&args[1..]),
        "prompt" => agent_prompt(&args[1..]),
        "rename" => agent_rename(&args[1..]),
        "focus" => agent_focus(&args[1..]),
        "wait" => agent_wait(&args[1..]),
        "attach" => agent_attach(&args[1..]),
        "start" => agent_start(&args[1..]),
        "explain" => agent_explain(&args[1..]),
        "help" | "--help" | "-h" => {
            print_agent_help();
            Ok(0)
        }
        _ => {
            print_agent_help();
            Ok(2)
        }
    }
}

fn agent_explain(args: &[String]) -> std::io::Result<i32> {
    let mut file = None;
    let mut agent = None;
    let mut json = false;
    let mut verbose = false;
    let mut target = None;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--file" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --file");
                    return Ok(2);
                };
                file = Some(value.clone());
                index += 2;
            }
            "--agent" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --agent");
                    return Ok(2);
                };
                agent = Some(value.clone());
                index += 2;
            }
            "--json" => {
                json = true;
                index += 1;
            }
            "--format" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --format");
                    return Ok(2);
                };
                match value.as_str() {
                    "json" => json = true,
                    "text" => json = false,
                    other => {
                        eprintln!("invalid --format: {other} (expected text or json)");
                        return Ok(2);
                    }
                }
                index += 2;
            }
            "--verbose" | "-v" => {
                verbose = true;
                index += 1;
            }
            "help" | "--help" | "-h" => {
                eprintln!("usage: herdr agent explain <target> [--json|--verbose]");
                eprintln!(
                    "usage: herdr agent explain --file PATH --agent LABEL [--json|--verbose]"
                );
                return Ok(0);
            }
            value if value.starts_with('-') => {
                eprintln!("unknown option: {value}");
                return Ok(2);
            }
            value => {
                if target.is_some() {
                    eprintln!("usage: herdr agent explain <target> [--json]");
                    return Ok(2);
                }
                target = Some(value.to_string());
                index += 1;
            }
        }
    }

    let explain = if let Some(path) = file {
        if target.is_some() {
            eprintln!("usage: herdr agent explain --file PATH --agent LABEL [--json]");
            return Ok(2);
        }
        let Some(agent_label) = agent else {
            eprintln!("herdr agent explain --file requires --agent LABEL");
            return Ok(2);
        };
        let content = std::fs::read_to_string(path)?;
        crate::detect::manifest::explain_to_json_value(&crate::detect::manifest::explain_for_label(
            &agent_label,
            &content,
        ))
    } else {
        let Some(target) = target else {
            eprintln!("usage: herdr agent explain <target> [--json]");
            eprintln!("usage: herdr agent explain --file PATH --agent LABEL [--json]");
            return Ok(2);
        };
        if agent.is_some() {
            eprintln!("--agent is only valid with --file");
            return Ok(2);
        }

        let response = super::send_request(&Request {
            id: "cli:agent:explain".into(),
            method: Method::AgentExplain(AgentTarget {
                target: target.to_owned(),
            }),
        })?;
        if response.get("error").is_some() {
            eprintln!("{}", serde_json::to_string(&response).unwrap());
            return Ok(1);
        }
        response["result"]["explain"].clone()
    };

    if json {
        println!("{explain}");
    } else {
        print_agent_explain_text(&explain, verbose);
    }
    Ok(0)
}

fn print_agent_explain_text(explain: &serde_json::Value, verbose: bool) {
    println!("agent: {}", explain["agent"].as_str().unwrap_or("unknown"));
    println!("state: {}", explain["state"].as_str().unwrap_or("unknown"));
    println!(
        "manifest: {} {}",
        explain["manifest_source"].as_str().unwrap_or("none"),
        explain["manifest_version"].as_str().unwrap_or("unknown")
    );
    if let Some(rule) = explain["matched_rule"].as_object() {
        let rule_id = rule
            .get("id")
            .and_then(|value| value.as_str())
            .unwrap_or("-");
        println!(
            "rule: {} (region={} priority={})",
            rule_id,
            rule.get("region")
                .and_then(|value| value.as_str())
                .unwrap_or("-"),
            rule.get("priority")
                .and_then(|value| value.as_i64())
                .unwrap_or(0),
        );
        if let Some(preview) = matched_rule_region_preview(explain, rule_id) {
            println!("evidence: {preview:?}");
        }
    } else {
        println!("rule: none");
    }
    if let Some(reason) = explain["fallback_reason"].as_str() {
        println!("fallback_reason: {reason}");
    }
    if let Some(reason) = explain["screen_detection_skip_reason"].as_str() {
        println!("screen_detection_skip_reason: {reason}");
    }
    if let Some(reason) = explain["skipped_update_reason"].as_str() {
        println!("skipped_update_reason: {reason}");
    }
    if let Some(warning) = explain["warning"].as_str() {
        println!("warning: {warning}");
    }

    if !verbose {
        return;
    }

    println!(
        "visible: idle={} blocker={} working={}",
        explain["visible_idle"].as_bool().unwrap_or(false),
        explain["visible_blocker"].as_bool().unwrap_or(false),
        explain["visible_working"].as_bool().unwrap_or(false)
    );
    println!(
        "cached_remote_version: {}",
        explain["cached_remote_version"].as_str().unwrap_or("none")
    );
    println!(
        "local_override_shadowing_remote: {}",
        explain["local_override_shadowing_remote"]
            .as_bool()
            .unwrap_or(false)
    );
    if let Some(status) = explain["remote_update_status"].as_str() {
        println!("remote_update_status: {status}");
    }
    if let Some(error) = explain["remote_update_error"].as_str() {
        println!("remote_update_error: {error}");
    }
    if let Some(evaluated_rules) = explain["evaluated_rules"]
        .as_array()
        .filter(|rules| !rules.is_empty())
    {
        println!("evaluated_rules:");
        for rule in evaluated_rules {
            println!(
                "  {} {} priority={} region={} state={}",
                if rule["matched"].as_bool().unwrap_or(false) {
                    "✓"
                } else {
                    "✗"
                },
                rule["id"].as_str().unwrap_or("-"),
                rule["priority"].as_i64().unwrap_or(0),
                rule["region"].as_str().unwrap_or("-"),
                rule["state"].as_str().unwrap_or("unknown")
            );
            let evidence = &rule["evidence"];
            println!(
                "    matchers: contains={:?} regex={:?} line_regex={:?} all={} any={} not={}",
                evidence["contains"],
                evidence["regex"],
                evidence["line_regex"],
                evidence["all_count"].as_u64().unwrap_or(0),
                evidence["any_count"].as_u64().unwrap_or(0),
                evidence["not_count"].as_u64().unwrap_or(0)
            );
            println!(
                "    region: bytes={} preview={:?}",
                evidence["region_bytes"].as_u64().unwrap_or(0),
                evidence["region_preview"].as_str().unwrap_or("")
            );
        }
    }
}

fn matched_rule_region_preview<'a>(
    explain: &'a serde_json::Value,
    rule_id: &str,
) -> Option<&'a str> {
    explain["evaluated_rules"]
        .as_array()?
        .iter()
        .find(|rule| rule["id"].as_str() == Some(rule_id))?["evidence"]["region_preview"]
        .as_str()
        .filter(|preview| !preview.is_empty())
}

fn agent_start(args: &[String]) -> std::io::Result<i32> {
    let Some(name) = args.first() else {
        eprintln!("usage: herdr agent start <name> [--cwd PATH] [--workspace ID] [--tab ID] [--split right|down] [--env KEY=VALUE] [--focus|--no-focus] -- <argv...>");
        return Ok(2);
    };

    let Some(separator) = args.iter().position(|arg| arg == "--") else {
        eprintln!("usage: herdr agent start <name> [--cwd PATH] [--workspace ID] [--tab ID] [--split right|down] [--env KEY=VALUE] [--focus|--no-focus] -- <argv...>");
        return Ok(2);
    };
    if separator == args.len() - 1 {
        eprintln!("agent start requires argv after --");
        return Ok(2);
    }

    let mut cwd = None;
    let mut workspace_id = None;
    let mut tab_id = None;
    let mut split = None;
    let mut focus = false;
    let mut env = HashMap::new();

    let mut index = 1;
    while index < separator {
        match args[index].as_str() {
            "--cwd" => {
                let Some(value) = args.get(index + 1).filter(|_| index + 1 < separator) else {
                    eprintln!("missing value for --cwd");
                    return Ok(2);
                };
                cwd = Some(value.clone());
                index += 2;
            }
            "--workspace" => {
                let Some(value) = args.get(index + 1).filter(|_| index + 1 < separator) else {
                    eprintln!("missing value for --workspace");
                    return Ok(2);
                };
                workspace_id = Some(super::normalize_workspace_id(value));
                index += 2;
            }
            "--tab" => {
                let Some(value) = args.get(index + 1).filter(|_| index + 1 < separator) else {
                    eprintln!("missing value for --tab");
                    return Ok(2);
                };
                tab_id = Some(super::normalize_tab_id(value));
                index += 2;
            }
            "--split" => {
                let Some(value) = args.get(index + 1).filter(|_| index + 1 < separator) else {
                    eprintln!("missing value for --split");
                    return Ok(2);
                };
                split = Some(super::parse_split_direction(value)?);
                index += 2;
            }
            "--focus" => {
                focus = true;
                index += 1;
            }
            "--no-focus" => {
                focus = false;
                index += 1;
            }
            "--env" => {
                let Some(value) = args.get(index + 1).filter(|_| index + 1 < separator) else {
                    eprintln!("missing value for --env");
                    return Ok(2);
                };
                let (key, value) = match super::parse_env_assignment(value) {
                    Ok(pair) => pair,
                    Err(err) => {
                        eprintln!("{err}");
                        return Ok(2);
                    }
                };
                env.insert(key, value);
                index += 2;
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:agent:start".into(),
        method: Method::AgentStart(AgentStartParams {
            name: name.clone(),
            cwd,
            workspace_id,
            tab_id,
            split,
            focus,
            argv: args[separator + 1..].to_vec(),
            env,
        }),
    })?)
}

fn agent_list(args: &[String]) -> std::io::Result<i32> {
    if !args.is_empty() {
        eprintln!("usage: herdr agent list");
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:agent:list".into(),
        method: Method::AgentList(EmptyParams::default()),
    })?)
}

fn agent_get(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = args.first() else {
        eprintln!("usage: herdr agent get <target>");
        return Ok(2);
    };
    if args.len() != 1 {
        eprintln!("usage: herdr agent get <target>");
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:agent:get".into(),
        method: Method::AgentGet(AgentTarget {
            target: target.clone(),
        }),
    })?)
}

fn agent_focus(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = args.first() else {
        eprintln!("usage: herdr agent focus <target>");
        return Ok(2);
    };
    if args.len() != 1 {
        eprintln!("usage: herdr agent focus <target>");
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:agent:focus".into(),
        method: Method::AgentFocus(AgentTarget {
            target: target.clone(),
        }),
    })?)
}

fn agent_attach(args: &[String]) -> std::io::Result<i32> {
    let (target, takeover) =
        match super::parse_attach_target(args, "usage: herdr agent attach <target> [--takeover]") {
            Ok(parsed) => parsed,
            Err(code) => return Ok(code),
        };

    let response = resolve_agent_target(&target, "cli:agent:attach:resolve")?;
    if response.get("error").is_some() {
        eprintln!("{}", serde_json::to_string(&response).unwrap());
        return Ok(1);
    }
    let Some(terminal_id) = response["result"]["agent"]["terminal_id"].as_str() else {
        eprintln!("agent attach failed: response did not include terminal_id");
        return Ok(1);
    };
    crate::client::run_terminal_attach(terminal_id.to_owned(), takeover)?;
    Ok(0)
}

fn agent_wait(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = args.first() else {
        eprintln!("usage: herdr agent wait <target> --status <idle|working|blocked|unknown> [--timeout MS]");
        return Ok(2);
    };

    let mut timeout_ms = None;
    let mut desired_status = None;

    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--status" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --status");
                    return Ok(2);
                };
                desired_status = Some(parse_agent_wait_status(value)?);
                index += 2;
            }
            "--timeout" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --timeout");
                    return Ok(2);
                };
                timeout_ms = Some(super::parse_u64_flag("--timeout", value)?);
                index += 2;
            }
            "help" | "--help" | "-h" => {
                eprintln!("usage: herdr agent wait <target> --status <idle|working|blocked|unknown> [--timeout MS]");
                return Ok(0);
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    let Some(agent_status) = desired_status else {
        eprintln!("missing required --status");
        return Ok(2);
    };

    let response = resolve_agent_target(target, "cli:agent:wait:resolve")?;
    if response.get("error").is_some() {
        eprintln!("{}", serde_json::to_string(&response).unwrap());
        return Ok(1);
    }
    if response["result"]["agent"]["agent_status"]
        .as_str()
        .is_some_and(|current| agent_wait_status_satisfied(agent_status, current))
    {
        println!("{}", serde_json::to_string(&response).unwrap());
        return Ok(0);
    }

    let Some(pane_id) = response["result"]["agent"]["pane_id"].as_str() else {
        eprintln!("agent wait failed: response did not include pane_id");
        return Ok(1);
    };

    let subscriptions = if agent_status == AgentStatus::Idle {
        vec![
            Subscription::PaneAgentStatusChanged {
                pane_id: pane_id.to_owned(),
                agent_status: Some(AgentStatus::Idle),
            },
            Subscription::PaneAgentStatusChanged {
                pane_id: pane_id.to_owned(),
                agent_status: Some(AgentStatus::Done),
            },
        ]
    } else {
        vec![Subscription::PaneAgentStatusChanged {
            pane_id: pane_id.to_owned(),
            agent_status: Some(agent_status),
        }]
    };

    super::wait_for_agent_change(
        Request {
            id: "cli:agent:wait".into(),
            method: Method::EventsSubscribe(crate::api::schema::EventsSubscribeParams {
                subscriptions,
            }),
        },
        timeout_ms,
        "timed out waiting for agent status change",
    )
}

fn resolve_agent_target(target: &str, request_id: &str) -> std::io::Result<serde_json::Value> {
    super::send_request(&Request {
        id: request_id.into(),
        method: Method::AgentGet(AgentTarget {
            target: target.to_owned(),
        }),
    })
}

fn agent_rename(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = args.first() else {
        eprintln!("usage: herdr agent rename <target> <name>|--clear");
        return Ok(2);
    };
    if args.len() < 2 {
        eprintln!("usage: herdr agent rename <target> <name>|--clear");
        return Ok(2);
    }
    let name = if args.len() == 2 && args[1] == "--clear" {
        None
    } else {
        Some(args[1..].join(" "))
    };

    super::print_response(&super::send_request(&Request {
        id: "cli:agent:rename".into(),
        method: Method::AgentRename(AgentRenameParams {
            target: target.clone(),
            name,
        }),
    })?)
}

fn agent_send(args: &[String]) -> std::io::Result<i32> {
    if args.len() < 2 {
        eprintln!("usage: herdr agent send <target> <text>");
        return Ok(2);
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:agent:send".into(),
        method: Method::AgentSend(AgentSendParams {
            target: args[0].clone(),
            text: args[1..].join(" "),
        }),
    })?)
}

fn agent_prompt(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = args.first() else {
        eprintln!(
            "usage: herdr agent prompt <target> <text> [--wait] [--until STATUS]... [--timeout MS]"
        );
        return Ok(2);
    };
    let Some(text) = args.get(1) else {
        eprintln!("agent prompt requires text");
        return Ok(2);
    };
    let mut wait = false;
    let mut until = Vec::new();
    let mut timeout_ms = None;
    let mut index = 2;
    while index < args.len() {
        match args[index].as_str() {
            "--wait" => {
                wait = true;
                index += 1;
            }
            "--until" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("--until requires at least one status");
                    return Ok(2);
                };
                until.push(parse_agent_wait_status(value)?);
                index += 2;
            }
            "--timeout" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --timeout");
                    return Ok(2);
                };
                timeout_ms = Some(super::parse_u64_flag("--timeout", value)?);
                index += 2;
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }
    if !until.is_empty() && !wait {
        eprintln!("--until requires --wait");
        return Ok(2);
    }
    if timeout_ms.is_some() && !wait {
        eprintln!("--timeout requires --wait");
        return Ok(2);
    }

    let prompt_request = Request {
        id: "cli:agent:prompt".into(),
        method: Method::AgentPrompt(AgentPromptParams {
            target: target.clone(),
            text: text.clone(),
        }),
    };

    if !wait {
        return super::print_response(&super::send_request(&prompt_request)?);
    }

    agent_prompt_wait(target, prompt_request, until, timeout_ms)
}

// Faithful adaptation of upstream's server-side `agent.prompt --wait`: the fork
// resolves settled state client-side (like `agent wait`), so the CLI subscribes
// to agent status changes, submits, then enforces the stall + settle semantics.
fn agent_prompt_wait(
    target: &str,
    prompt_request: Request,
    until: Vec<AgentStatus>,
    timeout_ms: Option<u64>,
) -> std::io::Result<i32> {
    // Baseline: whether the agent is already working decides if a stall gate applies.
    let baseline = resolve_agent_target(target, "cli:agent:prompt:resolve")?;
    if baseline.get("error").is_some() {
        eprintln!("{}", serde_json::to_string(&baseline).unwrap());
        return Ok(1);
    }
    let Some(pane_id) = baseline["result"]["agent"]["pane_id"].as_str() else {
        eprintln!("agent prompt failed: response did not include pane_id");
        return Ok(1);
    };
    let pane_id = pane_id.to_owned();
    let baseline_status = baseline["result"]["agent"]["agent_status"]
        .as_str()
        .unwrap_or("unknown")
        .to_owned();
    let baseline_working = baseline_status == "working";

    // Subscribe before submitting so a fast status change cannot slip through the
    // gap between the prompt landing and the wait starting.
    let effect_ms = timeout_ms.map_or(AGENT_PROMPT_EFFECT_TIMEOUT_MS, |timeout_ms| {
        timeout_ms.min(AGENT_PROMPT_EFFECT_TIMEOUT_MS)
    });
    let recv_timeout = Duration::from_millis(effect_ms.max(1));
    let subscribe = Request {
        id: "cli:agent:prompt:wait".into(),
        method: Method::EventsSubscribe(EventsSubscribeParams {
            subscriptions: vec![Subscription::PaneAgentStatusChanged {
                pane_id,
                agent_status: None,
            }],
        }),
    };
    let (ack, mut stream) = ApiClient::local()
        .subscribe_value(&subscribe, Some(recv_timeout))
        .map_err(super::api_client_error_to_io)?;
    if let Err(err) = crate::api::client::parse_response_value(ack) {
        if let ApiClientError::ErrorResponse(response) = err {
            eprintln!("{}", serde_json::to_string(&response).unwrap());
            return Ok(1);
        }
        return Err(super::api_client_error_to_io(err));
    }

    // Submit the prompt. Surface any submission error (empty text, no agent, ...).
    let prompt_response = super::send_request(&prompt_request)?;
    if prompt_response.get("error").is_some() {
        eprintln!("{}", serde_json::to_string(&prompt_response).unwrap());
        return Ok(1);
    }

    let wait_started = Instant::now();

    // Phase 1 — effect gate: from a non-working baseline, require any observed
    // state change within the effect window, else report stall (or timeout when
    // the caller's --timeout is the shorter bound).
    if !baseline_working {
        match stream.next_event() {
            Ok(Some(event)) => {
                if let Some(status) = event_agent_status(&event) {
                    if agent_prompt_status_settles(&until, status) {
                        return print_agent_prompted(target);
                    }
                }
            }
            Ok(None) => {
                eprintln!("subscription closed before the agent responded");
                return Ok(1);
            }
            Err(ApiClientError::Io(err)) if super::api_timeout_error(&err) => {
                return agent_prompt_stalled(timeout_ms, effect_ms, &baseline_status);
            }
            Err(err) => return Err(super::api_client_error_to_io(err)),
        }
    }

    // Phase 2 — settle: wait for a status that satisfies --until (default idle,
    // done, or blocked). The socket read timeout is a poll granularity; the overall
    // deadline is enforced against the caller's --timeout (indefinite when unset).
    loop {
        if let Some(total) = timeout_ms {
            if wait_started.elapsed() >= Duration::from_millis(total) {
                eprintln!(
                    "{}",
                    agent_prompt_error("timeout", "timed out waiting for agent status")
                );
                return Ok(1);
            }
        }
        match stream.next_event() {
            Ok(Some(event)) => {
                if let Some(status) = event_agent_status(&event) {
                    if agent_prompt_status_settles(&until, status) {
                        return print_agent_prompted(target);
                    }
                }
            }
            Ok(None) => {
                eprintln!("subscription closed before the agent settled");
                return Ok(1);
            }
            Err(ApiClientError::Io(err)) if super::api_timeout_error(&err) => {
                // Poll interval elapsed; loop re-checks the overall deadline.
            }
            Err(err) => return Err(super::api_client_error_to_io(err)),
        }
    }
}

fn event_agent_status(event: &SubscriptionEventEnvelope) -> Option<AgentStatus> {
    match &event.data {
        SubscriptionEventData::PaneAgentStatusChanged(changed) => Some(changed.agent_status),
        _ => None,
    }
}

fn agent_prompt_status_settles(until: &[AgentStatus], status: AgentStatus) -> bool {
    let desired: &[AgentStatus] = if until.is_empty() {
        &[AgentStatus::Idle, AgentStatus::Blocked]
    } else {
        until
    };
    desired.iter().any(|wanted| match wanted {
        AgentStatus::Idle | AgentStatus::Done => {
            matches!(status, AgentStatus::Idle | AgentStatus::Done)
        }
        AgentStatus::Working => status == AgentStatus::Working,
        AgentStatus::Blocked => status == AgentStatus::Blocked,
        AgentStatus::Unknown => status == AgentStatus::Unknown,
    })
}

fn agent_prompt_stalled(
    timeout_ms: Option<u64>,
    effect_ms: u64,
    baseline_status: &str,
) -> std::io::Result<i32> {
    let rendered = if agent_prompt_effect_code(timeout_ms) == "timeout" {
        agent_prompt_error("timeout", "timed out waiting for agent status")
    } else {
        agent_prompt_error(
            "agent_prompt_stalled",
            &format!(
                "agent prompt produced no observed state change within {effect_ms} ms; status is {baseline_status}"
            ),
        )
    };
    eprintln!("{rendered}");
    Ok(1)
}

// When the effect window elapses with no state change, a caller-supplied
// --timeout at or under the window reports `timeout`; otherwise it stalls.
fn agent_prompt_effect_code(timeout_ms: Option<u64>) -> &'static str {
    if timeout_ms.is_some_and(|timeout_ms| timeout_ms <= AGENT_PROMPT_EFFECT_TIMEOUT_MS) {
        "timeout"
    } else {
        "agent_prompt_stalled"
    }
}

fn agent_prompt_error(code: &str, message: &str) -> String {
    serde_json::to_string(&serde_json::json!({
        "id": "cli:agent:prompt",
        "error": { "code": code, "message": message },
    }))
    .unwrap_or_else(|_| format!("{{\"error\":{{\"code\":\"{code}\"}}}}"))
}

fn print_agent_prompted(target: &str) -> std::io::Result<i32> {
    let resolved = resolve_agent_target(target, "cli:agent:prompt:result")?;
    if resolved.get("error").is_some() {
        eprintln!("{}", serde_json::to_string(&resolved).unwrap());
        return Ok(1);
    }
    let agent = resolved["result"]["agent"].clone();
    let response = serde_json::json!({
        "id": "cli:agent:prompt",
        "result": { "type": "agent_prompted", "agent": agent },
    });
    println!("{}", serde_json::to_string(&response).unwrap());
    Ok(0)
}

fn agent_read(args: &[String]) -> std::io::Result<i32> {
    let Some(target) = args.first() else {
        eprintln!("usage: herdr agent read <target> [--source visible|recent|recent-unwrapped] [--lines N] [--format text|ansi] [--ansi]");
        return Ok(2);
    };

    let mut source = ReadSource::Recent;
    let mut lines = None;
    let mut format = ReadFormat::Text;
    let mut strip_ansi = true;

    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--source" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --source");
                    return Ok(2);
                };
                source = super::parse_read_source(value)?;
                index += 2;
            }
            "--lines" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --lines");
                    return Ok(2);
                };
                lines = Some(super::parse_u32_flag("--lines", value)?);
                index += 2;
            }
            "--format" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for --format");
                    return Ok(2);
                };
                format = super::parse_read_format(value)?;
                strip_ansi = !matches!(format, ReadFormat::Ansi);
                index += 2;
            }
            "--ansi" => {
                format = ReadFormat::Ansi;
                strip_ansi = false;
                index += 1;
            }
            other => {
                eprintln!("unknown option: {other}");
                return Ok(2);
            }
        }
    }

    super::print_response(&super::send_request(&Request {
        id: "cli:agent:read".into(),
        method: Method::AgentRead(AgentReadParams {
            target: target.clone(),
            source,
            lines,
            format,
            strip_ansi,
        }),
    })?)
}

fn agent_wait_status_satisfied(desired: AgentStatus, current: &str) -> bool {
    match desired {
        AgentStatus::Idle => matches!(current, "idle" | "done"),
        AgentStatus::Working => current == "working",
        AgentStatus::Blocked => current == "blocked",
        AgentStatus::Unknown => current == "unknown",
        AgentStatus::Done => false,
    }
}

fn parse_agent_wait_status(value: &str) -> std::io::Result<AgentStatus> {
    match value {
        "idle" => Ok(AgentStatus::Idle),
        "working" => Ok(AgentStatus::Working),
        "blocked" => Ok(AgentStatus::Blocked),
        "unknown" => Ok(AgentStatus::Unknown),
        "done" => Err(std::io::Error::other(
            "done is a UI attention state; use idle for CLI agent completion waits",
        )),
        _ => Err(std::io::Error::other(format!(
            "invalid agent status: {value} (expected idle, working, blocked, or unknown)"
        ))),
    }
}

fn print_agent_help() {
    eprintln!("herdr agent commands:");
    eprintln!("  herdr agent list");
    eprintln!("  herdr agent get <target>");
    eprintln!("  herdr agent read <target> [--source visible|recent|recent-unwrapped] [--lines N] [--format text|ansi] [--ansi]");
    eprintln!("  herdr agent send <target> <text>");
    eprintln!(
        "  herdr agent prompt <target> <text> [--wait] [--until STATUS]... [--timeout MS]"
    );
    eprintln!("  herdr agent rename <target> <name>|--clear");
    eprintln!("  herdr agent focus <target>");
    eprintln!("  herdr agent wait <target> --status <idle|working|blocked|unknown> [--timeout MS]");
    eprintln!("  herdr agent attach <target> [--takeover]");
    eprintln!("  herdr agent start <name> [--cwd PATH] [--workspace ID] [--tab ID] [--split right|down] [--env KEY=VALUE] [--focus|--no-focus] -- <argv...>");
    eprintln!("  herdr agent explain <target> [--json]");
    eprintln!("  herdr agent explain --file PATH --agent LABEL [--json]");
    eprintln!("  targets accept terminal ids, unique agent names, detected/reported agent labels, and legacy pane ids");
    eprintln!(
        "  agent send writes literal text; agent prompt submits text plus Enter atomically"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settle_matches_idle_done_and_blocked() {
        assert!(agent_prompt_status_settles(&[], AgentStatus::Idle));
        assert!(agent_prompt_status_settles(&[], AgentStatus::Done));
        assert!(agent_prompt_status_settles(&[], AgentStatus::Blocked));
        assert!(!agent_prompt_status_settles(&[], AgentStatus::Working));
        assert!(!agent_prompt_status_settles(&[], AgentStatus::Unknown));
    }

    #[test]
    fn explicit_until_matches_only_requested_states() {
        let until = vec![AgentStatus::Working];
        assert!(agent_prompt_status_settles(&until, AgentStatus::Working));
        assert!(!agent_prompt_status_settles(&until, AgentStatus::Idle));
    }

    #[test]
    fn until_idle_also_accepts_done() {
        let until = vec![AgentStatus::Idle];
        assert!(agent_prompt_status_settles(&until, AgentStatus::Done));
    }

    #[test]
    fn effect_code_is_timeout_only_when_caller_timeout_is_shorter() {
        assert_eq!(agent_prompt_effect_code(None), "agent_prompt_stalled");
        assert_eq!(agent_prompt_effect_code(Some(10_000)), "agent_prompt_stalled");
        assert_eq!(
            agent_prompt_effect_code(Some(AGENT_PROMPT_EFFECT_TIMEOUT_MS)),
            "timeout"
        );
        assert_eq!(agent_prompt_effect_code(Some(1_000)), "timeout");
    }

    #[test]
    fn error_payload_carries_code_and_message() {
        let rendered = agent_prompt_error("agent_prompt_stalled", "no change");
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["error"]["code"], "agent_prompt_stalled");
        assert_eq!(value["error"]["message"], "no change");
    }
}
