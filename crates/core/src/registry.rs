use crate::context::Context;

#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub name: &'static str,
    pub category: &'static str,
    pub summary: &'static str,
    pub aliases: &'static [&'static str],
    pub subcommands: &'static [SubcommandSpec],
    pub handler: fn(&Context),
}

#[derive(Debug, Clone)]
pub struct SubcommandSpec {
    pub name: &'static str,
    pub summary: &'static str,
    pub aliases: &'static [&'static str],
    pub handler: fn(&Context),
}

#[derive(Debug, Clone)]
pub struct FlagSpec {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
}

#[derive(Debug, Clone)]
pub struct ParamSpec {
    pub name: &'static str,
    pub description: &'static str,
}

#[derive(Debug, Default, Clone)]
pub struct Registry {
    commands: Vec<CommandSpec>,
    flags: Vec<FlagSpec>,
    params: Vec<ParamSpec>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_command(&mut self, command: CommandSpec) {
        self.commands.push(command);
    }

    pub fn add_flag(&mut self, flag: FlagSpec) {
        self.flags.push(flag);
    }

    pub fn add_param(&mut self, param: ParamSpec) {
        self.params.push(param);
    }

    pub fn commands(&self) -> &[CommandSpec] {
        &self.commands
    }

    pub fn flags(&self) -> &[FlagSpec] {
        &self.flags
    }

    pub fn params(&self) -> &[ParamSpec] {
        &self.params
    }

    pub fn command_for(&self, token: &str) -> Option<&CommandSpec> {
        self.commands
            .iter()
            .find(|command| command.name == token || command.aliases.contains(&token))
    }

    pub fn command_named(&self, name: &str) -> Option<&CommandSpec> {
        self.commands.iter().find(|command| command.name == name)
    }

    pub fn subcommand_for<'a>(
        &'a self,
        command: &'a CommandSpec,
        token: &str,
    ) -> Option<&'a SubcommandSpec> {
        command
            .subcommands
            .iter()
            .find(|subcommand| subcommand.name == token || subcommand.aliases.contains(&token))
    }

    pub fn subcommand_named<'a>(
        &'a self,
        command: &'a CommandSpec,
        name: &str,
    ) -> Option<&'a SubcommandSpec> {
        command
            .subcommands
            .iter()
            .find(|subcommand| subcommand.name == name)
    }

    pub fn suggestion_tokens(&self) -> Vec<String> {
        let mut tokens = Vec::new();
        for command in &self.commands {
            tokens.push(command.name.to_string());
            for alias in command.aliases {
                tokens.push(alias.to_string());
            }
            for sub in command.subcommands {
                tokens.push(sub.name.to_string());
                for alias in sub.aliases {
                    tokens.push(alias.to_string());
                }
            }
        }
        for flag in &self.flags {
            tokens.push(flag.name.to_string());
            for alias in flag.aliases {
                tokens.push(alias.to_string());
            }
        }
        for param in &self.params {
            tokens.push(param.name.to_string());
        }
        tokens
    }
}
