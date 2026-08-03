mod auth;
mod client;
mod cmd;
mod markdown;
mod mime;
mod note;
mod output;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "plane",
    version,
    about = "CLI for a self-hosted Plane CE instance",
    after_help = "Auth: PLANE_API_KEY, else a PAT read from Proton Pass (PLANE_PASS_VAULT, PLANE_PASS_ITEM, PLANE_PASS_FIELD).\nEnv: PLANE_WORKSPACE (required), PLANE_API_BASE, PLANE_WEB_BASE."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Print the raw API response instead of rendered lines
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Read and write issues
    #[command(alias = "issues", subcommand)]
    Issue(IssueCmd),

    /// List projects (a project is a durable area: RES, LOC, TEACH, ...)
    #[command(subcommand)]
    Project(ProjectCmd),

    /// List the modules (deliverables) of a project
    #[command(subcommand)]
    Module(ModuleCmd),

    /// List the workflow states of a project
    #[command(subcommand)]
    State(StateCmd),

    /// List the labels of a project
    #[command(subcommand)]
    Label(LabelCmd),
}

#[derive(Subcommand)]
enum IssueCmd {
    /// Show one issue by its identifier (e.g. RES-12)
    Get {
        /// Issue reference, e.g. RES-12
        reference: String,
    },

    /// List the issues of a project
    List {
        /// Project identifier, e.g. RES
        project: String,

        /// Only issues in this state (Backlog, Todo, In Progress, Done, Cancelled)
        #[arg(long)]
        state: Option<String>,

        /// Only issues in this module, by name
        #[arg(long)]
        module: Option<String>,

        /// Only issues carrying this label, by name
        #[arg(long)]
        label: Option<String>,
    },

    /// Create an issue
    Create {
        /// Project identifier, e.g. RES. Omit when using --from-note.
        project: Option<String>,

        /// Issue title
        title: Option<String>,

        /// Take the project (and module) from an Obsidian note's frontmatter
        #[arg(long, value_name = "NOTE")]
        from_note: Option<PathBuf>,

        /// Attach to this module, by name (overrides the note's module)
        #[arg(long)]
        module: Option<String>,

        /// Initial state (default: the project's own default)
        #[arg(long)]
        state: Option<String>,

        /// urgent, high, medium, low, none
        #[arg(long)]
        priority: Option<String>,

        /// Due date, YYYY-MM-DD
        #[arg(long)]
        due: Option<String>,

        /// Description as markdown; `-` reads stdin. Converted to HTML on write.
        #[arg(long, value_name = "MARKDOWN")]
        desc_md: Option<String>,

        /// Label to set, by name. Repeatable.
        #[arg(long = "label", value_name = "LABEL")]
        labels: Vec<String>,
    },

    /// Update an issue. Closing one is `--state done`.
    Update {
        /// Issue reference, e.g. RES-12
        reference: String,

        /// New state (Backlog, Todo, In Progress, Done, Cancelled)
        #[arg(long)]
        state: Option<String>,

        /// urgent, high, medium, low, none
        #[arg(long)]
        priority: Option<String>,

        /// Due date, YYYY-MM-DD
        #[arg(long)]
        due: Option<String>,

        /// New title
        #[arg(long)]
        title: Option<String>,

        /// Move into this module, by name
        #[arg(long)]
        module: Option<String>,

        /// Label to set, by name. Repeatable, and replaces the issue's
        /// current labels rather than adding to them.
        #[arg(long = "label", value_name = "LABEL")]
        labels: Vec<String>,
    },

    /// Comment on an issue
    Comment {
        /// Issue reference, e.g. RES-12
        reference: String,

        /// Comment as markdown; `-` reads stdin
        text: String,
    },

    /// Upload one or more files to an issue
    Attach {
        /// Issue reference, e.g. RES-12
        reference: String,

        /// Files to upload
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },

    /// List the files attached to an issue
    Attachments {
        /// Issue reference, e.g. RES-12
        reference: String,
    },
}

#[derive(Subcommand)]
enum ProjectCmd {
    /// List every project in the workspace
    List,
}

#[derive(Subcommand)]
enum ModuleCmd {
    /// List the modules of a project
    List {
        /// Project identifier, e.g. RES
        project: String,
    },
}

#[derive(Subcommand)]
enum StateCmd {
    /// List the states of a project
    List {
        /// Project identifier, e.g. RES
        project: String,
    },
}

#[derive(Subcommand)]
enum LabelCmd {
    /// List the labels of a project
    List {
        /// Project identifier, e.g. RES
        project: String,
    },
}

fn main() {
    let cli = Cli::parse();
    let json = cli.json;

    let result = match cli.command {
        Commands::Issue(issue) => match issue {
            IssueCmd::Get { reference } => cmd::issue_get(&reference, json),
            IssueCmd::List {
                project,
                state,
                module,
                label,
            } => cmd::issue_list(
                &project,
                state.as_deref(),
                module.as_deref(),
                label.as_deref(),
                json,
            ),
            IssueCmd::Create {
                project,
                title,
                from_note,
                module,
                state,
                priority,
                due,
                desc_md,
                labels,
            } => cmd::issue_create(cmd::CreateArgs {
                project,
                title,
                from_note,
                module,
                state,
                priority,
                due,
                desc_md,
                labels,
                json,
            }),
            IssueCmd::Update {
                reference,
                state,
                priority,
                due,
                title,
                module,
                labels,
            } => cmd::issue_update(cmd::UpdateArgs {
                reference,
                state,
                priority,
                due,
                title,
                module,
                labels,
                json,
            }),
            IssueCmd::Comment { reference, text } => cmd::issue_comment(&reference, &text, json),
            IssueCmd::Attach { reference, files } => cmd::issue_attach(&reference, &files, json),
            IssueCmd::Attachments { reference } => cmd::issue_attachments(&reference, json),
        },
        Commands::Project(ProjectCmd::List) => cmd::project_list(json),
        Commands::Module(ModuleCmd::List { project }) => cmd::module_list(&project, json),
        Commands::State(StateCmd::List { project }) => cmd::state_list(&project, json),
        Commands::Label(LabelCmd::List { project }) => cmd::label_list(&project, json),
    };

    if let Err(e) = result {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn from_note_puts_the_title_in_the_first_positional() {
        // `plane issue create --from-note n.md "title"` has one positional,
        // and clap fills `project` first; cmd::issue_create swaps it back.
        let cli =
            Cli::try_parse_from(["plane", "issue", "create", "--from-note", "n.md", "T"]).unwrap();
        match cli.command {
            Commands::Issue(IssueCmd::Create {
                project,
                title,
                from_note,
                ..
            }) => {
                assert_eq!(project.as_deref(), Some("T"));
                assert_eq!(title, None);
                assert!(from_note.is_some());
            }
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn plain_create_takes_project_then_title() {
        let cli = Cli::try_parse_from(["plane", "issue", "create", "RES", "T"]).unwrap();
        match cli.command {
            Commands::Issue(IssueCmd::Create { project, title, .. }) => {
                assert_eq!(project.as_deref(), Some("RES"));
                assert_eq!(title.as_deref(), Some("T"));
            }
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn json_is_global_and_works_after_the_subcommand() {
        let cli = Cli::try_parse_from(["plane", "issue", "get", "RES-12", "--json"]).unwrap();
        assert!(cli.json);
    }

    #[test]
    fn label_is_repeatable_on_create_and_update() {
        let cli = Cli::try_parse_from([
            "plane", "issue", "update", "RES-50", "--label", "waiting", "--label", "deep",
        ])
        .unwrap();
        match cli.command {
            Commands::Issue(IssueCmd::Update { labels, .. }) => {
                // Both have to survive parsing: the API takes the whole set in
                // one array, so a flag that only kept the last one would
                // silently drop a label.
                assert_eq!(labels, vec!["waiting".to_string(), "deep".to_string()]);
            }
            _ => panic!("wrong command"),
        }
        let cli = Cli::try_parse_from(["plane", "issue", "create", "RES", "T", "--label", "quick"])
            .unwrap();
        match cli.command {
            Commands::Issue(IssueCmd::Create { labels, .. }) => {
                assert_eq!(labels, vec!["quick".to_string()]);
            }
            _ => panic!("wrong command"),
        }
    }

    #[test]
    fn attach_takes_a_reference_and_at_least_one_file() {
        let cli =
            Cli::try_parse_from(["plane", "issue", "attach", "RES-50", "a.pdf", "b.png"]).unwrap();
        match cli.command {
            Commands::Issue(IssueCmd::Attach { reference, files }) => {
                assert_eq!(reference, "RES-50");
                assert_eq!(files.len(), 2);
            }
            _ => panic!("wrong command"),
        }
        // A reference with no file is a no-op, so clap refuses it.
        assert!(Cli::try_parse_from(["plane", "issue", "attach", "RES-50"]).is_err());
    }

    #[test]
    fn there_is_no_close_subcommand() {
        // Closing is `--state done`; a second spelling would add a concept.
        assert!(Cli::try_parse_from(["plane", "issue", "close", "RES-12"]).is_err());
    }
}
