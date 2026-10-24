//! `paddock shell`: a session with a node. Each line is a
//! `paddock` command run against the node the session is connected to;
//! `connect` moves the session to another node from the contacts file
//! ([`super::contacts`]). Ctrl-C ends the command running (a follow, a
//! slow call), not the session; Ctrl-D or `exit` ends the session.

use std::path::PathBuf;

use anyhow::Context;
use clap::{CommandFactory, Parser, Subcommand};
use paddock::api::Client;
use rustyline::completion::Completer;
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::FileHistory;
use rustyline::validate::Validator;
use rustyline::{Config, Editor, Helper};

use super::contacts::{self, Contacts};
use super::format;
use crate::{Cli, Command};

/// One line of the shell.
#[derive(Debug, Parser)]
#[command(
    name = "paddock",
    no_binary_name = true,
    disable_version_flag = true,
    help_template = "{subcommands}\n\nEach command takes --help."
)]
struct Line {
    /// Print JSON instead of text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: ShellCommand,
}

#[derive(Debug, Subcommand)]
enum ShellCommand {
    /// Move the session to another node: a contact's name or node id, or
    /// `local` for the admin socket.
    Connect {
        /// Contact name, node id, or `local`.
        node: String,
    },
    /// List the contacts file's nodes.
    Contacts,
    /// Leave the shell (or Ctrl-D).
    #[command(alias = "quit")]
    Exit,
    #[command(flatten)]
    Admin(Command),
}

/// What the loop does after a line.
#[derive(Debug, PartialEq, Eq)]
enum Flow {
    Continue,
    Exit,
}

/// The node a session talks to, and how to reach others.
struct Session {
    client: Client,
    /// The prompt's name for the node.
    label: String,
    contacts: Contacts,
    socket: PathBuf,
    /// The operator's private key, for `connect`.
    key: Option<PathBuf>,
    json: bool,
}

/// Run the shell until `exit` or Ctrl-D.
///
/// # Errors
///
/// An unreadable contacts file, or a terminal the line editor cannot use.
pub(crate) fn run(cli: &Cli, contacts_path: Option<&std::path::Path>) -> anyhow::Result<()> {
    let contacts = Contacts::load(contacts_path)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?;
    let mut session = Session {
        client: crate::client(cli)?,
        label: first_label(cli, &contacts),
        socket: cli.socket.clone(),
        key: operator_key(cli).or_else(|| contacts.key.clone()),
        json: cli.json,
        contacts,
    };
    let mut editor: Editor<Completions, FileHistory> =
        Editor::with_config(Config::builder().auto_add_history(true).build())?;
    editor.set_helper(Some(Completions {
        commands: command_names(),
        targets: session.targets(),
    }));
    let history = contacts::config_dir().map(|d| d.join("history"));
    if let Some(h) = &history {
        let _ = editor.load_history(h);
    }
    println!("paddock shell: `help` lists the commands; Ctrl-C stops one, Ctrl-D leaves");
    loop {
        match editor.readline(&format!("paddock({})> ", session.label)) {
            Ok(line) => {
                if session.line(&rt, &line) == Flow::Exit {
                    break;
                }
            }
            Err(ReadlineError::Interrupted) => {}
            Err(ReadlineError::Eof) => break,
            Err(e) => return Err(e.into()),
        }
    }
    if let Some(h) = &history {
        if let Some(dir) = h.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = editor.save_history(h) {
            eprintln!("paddock: cannot save the history to {}: {e}", h.display());
        }
    }
    Ok(())
}

#[cfg(feature = "tls")]
fn operator_key(cli: &Cli) -> Option<PathBuf> {
    cli.key.clone()
}

#[cfg(not(feature = "tls"))]
fn operator_key(_cli: &Cli) -> Option<PathBuf> {
    None
}

/// The prompt's name for the node the options reach: a contact's name for
/// its address, the address, or `local`.
#[cfg(feature = "tls")]
fn first_label(cli: &Cli, contacts: &Contacts) -> String {
    match &cli.remote {
        Some(address) => contacts
            .at(address)
            .map_or_else(|| address.clone(), |c| c.name.clone()),
        None => "local".into(),
    }
}

#[cfg(not(feature = "tls"))]
fn first_label(_cli: &Cli, _contacts: &Contacts) -> String {
    "local".into()
}

/// Every command's name and visible alias, for completion.
fn command_names() -> Vec<String> {
    let mut names: Vec<String> = Line::command()
        .get_subcommands()
        .flat_map(|c| {
            std::iter::once(c.get_name().to_string()).chain(c.get_all_aliases().map(str::to_string))
        })
        .filter(|n| n != "node" && n != "shell")
        .collect();
    names.push("help".into());
    names.sort();
    names.dedup();
    names
}

impl Session {
    /// What `connect` takes: `local` and each contact's name.
    fn targets(&self) -> Vec<String> {
        std::iter::once("local".to_string())
            .chain(self.contacts.nodes.iter().map(|c| c.name.clone()))
            .collect()
    }

    /// Run one line.
    fn line(&mut self, rt: &tokio::runtime::Runtime, line: &str) -> Flow {
        let words = match split_words(line) {
            Ok(w) if w.is_empty() => return Flow::Continue,
            Ok(w) => w,
            Err(e) => {
                eprintln!("paddock: {e}");
                return Flow::Continue;
            }
        };
        let parsed = match Line::try_parse_from(words) {
            Ok(p) => p,
            Err(e) => {
                let _ = e.print();
                return Flow::Continue;
            }
        };
        let json = self.json || parsed.json;
        let result = match parsed.command {
            ShellCommand::Exit => return Flow::Exit,
            ShellCommand::Connect { node } => rt.block_on(self.connect(&node)),
            ShellCommand::Contacts => {
                self.print_contacts();
                Ok(())
            }
            ShellCommand::Admin(Command::Shell { .. }) => {
                Err(anyhow::anyhow!("already in the shell"))
            }
            #[cfg(feature = "node")]
            ShellCommand::Admin(Command::Node { .. }) => Err(anyhow::anyhow!(
                "`node` runs a node in this process; run it outside the shell"
            )),
            ShellCommand::Admin(Command::Members) if !json => rt.block_on(interruptible(async {
                let members = self.client.members().await?;
                println!(
                    "{}",
                    format::members_named(&members, &self.contacts.names_by_id())
                );
                Ok(())
            })),
            ShellCommand::Admin(command) => {
                rt.block_on(interruptible(crate::run(&self.client, json, command)))
            }
        };
        if let Err(e) = result {
            eprintln!("paddock: {e:#}");
        }
        Flow::Continue
    }

    /// Move the session to `which`, if that node answers.
    async fn connect(&mut self, which: &str) -> anyhow::Result<()> {
        let (client, label) = if which == "local" {
            (Client::new(&self.socket), "local".to_string())
        } else {
            let contact = self.contacts.find(which).with_context(|| {
                format!("no contact {which}: `contacts` lists them, `local` is the admin socket")
            })?;
            let key = self.key.as_deref().context(
                "connecting needs the operator's private key: --key, or `key` in the contacts file",
            )?;
            let client = crate::remote_client(&contact.address, &contact.node_key, key)?;
            (client, contact.name.clone())
        };
        client
            .members()
            .await
            .with_context(|| format!("{label} did not answer; still on {}", self.label))?;
        self.client = client;
        self.label = label;
        println!("connected to {}", self.label);
        Ok(())
    }

    fn print_contacts(&self) {
        if self.contacts.nodes.is_empty() {
            let at = contacts::default_path()
                .map_or_else(|| "--contacts".into(), |p| p.display().to_string());
            println!("no contacts (they are read from {at})");
            return;
        }
        let rows: Vec<Vec<String>> = self
            .contacts
            .nodes
            .iter()
            .map(|c| {
                vec![
                    c.name.clone(),
                    c.id.map_or_else(|| "-".into(), |i| i.to_string()),
                    c.address.clone(),
                ]
            })
            .collect();
        println!("{}", format::table(&["NAME", "NODE", "ADDRESS"], &rows));
    }
}

/// Run `work` until it ends or Ctrl-C.
async fn interruptible(work: impl Future<Output = anyhow::Result<()>>) -> anyhow::Result<()> {
    tokio::select! {
        r = work => r,
        _ = tokio::signal::ctrl_c() => {
            println!();
            Ok(())
        }
    }
}

/// Split a line into words as a shell would: whitespace separates them,
/// single quotes keep everything literally, double quotes keep everything
/// but `\"` and `\\`, and a backslash outside quotes escapes the next
/// character.
pub(crate) fn split_words(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err("unterminated '".into()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\')) => word.push(c),
                            Some(c) => {
                                word.push('\\');
                                word.push(c);
                            }
                            None => return Err("unterminated \"".into()),
                        },
                        Some(c) => word.push(c),
                        None => return Err("unterminated \"".into()),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some(c) => word.push(c),
                    None => return Err("a line may not end with \\".into()),
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

/// Completion for the shell: command names first, then contacts after
/// `connect`.
struct Completions {
    commands: Vec<String>,
    targets: Vec<String>,
}

/// Where the word before the cursor starts in `before`, and what it may
/// complete to.
fn complete(commands: &[String], targets: &[String], before: &str) -> (usize, Vec<String>) {
    let start = before.rfind(char::is_whitespace).map_or(0, |i| i + 1);
    let word = &before[start..];
    let earlier: Vec<&str> = before[..start].split_whitespace().collect();
    let pool = match earlier.as_slice() {
        [] => commands,
        ["connect"] => targets,
        _ => return (start, Vec::new()),
    };
    let found = pool.iter().filter(|c| c.starts_with(word)).cloned();
    (start, found.collect())
}

impl Completer for Completions {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        Ok(complete(&self.commands, &self.targets, &line[..pos]))
    }
}

impl Hinter for Completions {
    type Hint = String;
}

impl Highlighter for Completions {}

impl Validator for Completions {}

impl Helper for Completions {}

#[cfg(test)]
#[path = "shell_tests.rs"]
mod shell_tests;
