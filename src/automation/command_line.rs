//! A program job's command line as one text (`php -r "print('x');"`), read
//! into the program and the arguments the definition holds, and written
//! back from them.
//!
//! A program starts **without a shell** (specification, section 9.1.2), so
//! this is not a shell's grammar: only quotes group words, and nothing else
//! is interpreted. The rules are the same on every platform, so that a job
//! reads the same wherever it is opened:
//!
//! - Words are separated by whitespace.
//! - `'…'` keeps everything between the quotes as it is.
//! - `"…"` keeps everything as it is, except `\"` (a quote) and `\\` (a
//!   backslash).
//! - Outside quotes a backslash is an ordinary character, so a Windows path
//!   (`C:\tools\run.exe`, `\\server\share`) needs no escaping.
//! - Quoted and unquoted pieces that touch are one word: `--name="a b"` is
//!   `--name=a b`. `""` is an empty argument.
//!
//! Pipes, redirections, `&&`, `;` and `$NAME` do nothing: they reach the
//! program as arguments. [`Command::shell_syntax`] finds them so the editor
//! can say so.

/// Why a command line cannot be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitError {
    /// A quote (`'` or `"`) is opened and never closed.
    UnclosedQuote(char),
}

/// A command line read into its words.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Command {
    /// The program, then each argument.
    pub words: Vec<String>,
    /// The first word written outside quotes that a shell would act on
    /// (`|`, `>`, `&&`, `$HOME`…), which here is passed as it is.
    pub shell_syntax: Option<String>,
}

impl Command {
    /// The program: the first word, or nothing for an empty line.
    pub fn program(&self) -> &str {
        self.words.first().map_or("", String::as_str)
    }

    /// Every word after the program.
    pub fn arguments(&self) -> &[String] {
        self.words.get(1..).unwrap_or_default()
    }
}

/// Reads a command line into its words (see the module's rules).
pub fn split(line: &str) -> Result<Command, SplitError> {
    let mut command = Command::default();
    let mut word = String::new();
    // Whether a word is being read (an empty `""` is one), and whether all
    // of it was written outside quotes.
    let mut reading = false;
    let mut bare = true;
    let mut finish = |word: &mut String, reading: &mut bool, bare: &mut bool| {
        if *reading {
            if *bare && command.shell_syntax.is_none() && is_shell_syntax(word) {
                command.shell_syntax = Some(word.clone());
            }
            command.words.push(std::mem::take(word));
        }
        *reading = false;
        *bare = true;
    };
    let mut characters = line.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            quote @ ('\'' | '"') => {
                reading = true;
                bare = false;
                loop {
                    match characters.next() {
                        None => return Err(SplitError::UnclosedQuote(quote)),
                        Some(next) if next == quote => break,
                        Some('\\') if quote == '"' => match characters.peek() {
                            Some(&escaped @ ('"' | '\\')) => {
                                characters.next();
                                word.push(escaped);
                            }
                            _ => word.push('\\'),
                        },
                        Some(next) => word.push(next),
                    }
                }
            }
            space if space.is_whitespace() => finish(&mut word, &mut reading, &mut bare),
            other => {
                reading = true;
                word.push(other);
            }
        }
    }
    finish(&mut word, &mut reading, &mut bare);
    Ok(command)
}

/// Whether a word written outside quotes is something a shell would act
/// on: an operator on its own, a redirection, or a variable.
fn is_shell_syntax(word: &str) -> bool {
    const OPERATORS: &[&str] = &[
        "|", "||", "&", "&&", ";", ">", ">>", "<", "<<", "2>", "2>&1",
    ];
    if OPERATORS.contains(&word) {
        return true;
    }
    if word.starts_with('>') || word.starts_with("2>") {
        return true;
    }
    // `$NAME`, `${NAME}` and `$(…)` at the start of a word.
    let mut characters = word.chars();
    characters.next() == Some('$')
        && characters
            .next()
            .is_some_and(|next| next.is_ascii_alphabetic() || matches!(next, '_' | '{' | '('))
}

/// Writes a program and its arguments as one command line that [`split`]
/// reads back into the same words.
pub fn join(program: &str, arguments: &[String]) -> String {
    let mut line = String::new();
    for word in std::iter::once(program).chain(arguments.iter().map(String::as_str)) {
        if !line.is_empty() {
            line.push(' ');
        }
        push_word(&mut line, word);
    }
    line
}

fn push_word(line: &mut String, word: &str) {
    let plain = !word.is_empty()
        && !word
            .chars()
            .any(|character| character.is_whitespace() || matches!(character, '\'' | '"'));
    if plain {
        line.push_str(word);
    } else if !word.contains('\'') {
        line.push('\'');
        line.push_str(word);
        line.push('\'');
    } else {
        line.push('"');
        for character in word.chars() {
            if matches!(character, '"' | '\\') {
                line.push('\\');
            }
            line.push(character);
        }
        line.push('"');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        split(line).map(|command| command.words).unwrap_or_default()
    }

    #[test]
    fn quotes_group_words_and_nothing_else_is_read() {
        assert_eq!(
            words(r#"php -r "print('teste');""#),
            ["php", "-r", "print('teste');"]
        );
        assert_eq!(
            words("python3 -c 'print(\"a b\")'"),
            ["python3", "-c", "print(\"a b\")"]
        );
        assert_eq!(
            words("  node   export.js\t--format  csv "),
            ["node", "export.js", "--format", "csv"]
        );
        assert_eq!(words(r#"tool --name="a b" """#), ["tool", "--name=a b", ""]);
        assert_eq!(words(r#"say "a \"b\" \\ c""#), ["say", r#"a "b" \ c"#]);
        // Only a quote and a backslash are escaped inside double quotes.
        assert_eq!(words(r#"say "a\nb""#), ["say", r"a\nb"]);
        assert!(words("").is_empty());
        assert!(words("   ").is_empty());
    }

    #[test]
    fn a_backslash_outside_quotes_is_part_of_the_word() {
        assert_eq!(
            words(r"C:\tools\run.exe \\server\share\in.csv"),
            [r"C:\tools\run.exe", r"\\server\share\in.csv"]
        );
        assert_eq!(
            words(r#""C:\Program Files\PHP\php.exe" -v"#),
            [r"C:\Program Files\PHP\php.exe", "-v"]
        );
    }

    #[test]
    fn an_unclosed_quote_is_an_error_naming_the_quote() {
        assert_eq!(
            split(r#"php -r "print('teste')"#),
            Err(SplitError::UnclosedQuote('"'))
        );
        assert_eq!(split("echo 'a"), Err(SplitError::UnclosedQuote('\'')));
    }

    #[test]
    fn the_program_is_the_first_word() {
        let command = split("php -r 'x'").unwrap_or_default();
        assert_eq!(command.program(), "php");
        assert_eq!(command.arguments(), ["-r", "x"]);
        let empty = split("").unwrap_or_default();
        assert_eq!(empty.program(), "");
        assert!(empty.arguments().is_empty());
    }

    #[test]
    fn what_a_shell_would_act_on_is_found_only_outside_quotes() {
        let syntax = |line: &str| split(line).unwrap_or_default().shell_syntax;
        assert_eq!(syntax("cat a.txt | wc -l").as_deref(), Some("|"));
        assert_eq!(syntax("tool > out.txt").as_deref(), Some(">"));
        assert_eq!(syntax("tool >out.txt").as_deref(), Some(">out.txt"));
        assert_eq!(syntax("a && b").as_deref(), Some("&&"));
        assert_eq!(syntax("echo $HOME").as_deref(), Some("$HOME"));
        assert_eq!(syntax("echo ${HOME}/x").as_deref(), Some("${HOME}/x"));
        assert_eq!(syntax("echo '$HOME' \"a | b\" 'x > y'"), None);
        assert_eq!(syntax("php -r 'print(1);'"), None);
        // A price is not a variable.
        assert_eq!(syntax("pay $5 a>b"), None);
    }

    #[test]
    fn a_joined_command_reads_back_into_the_same_words() {
        assert_eq!(
            join("php", &["-r".into(), "print('teste');".into()]),
            r#"php -r "print('teste');""#
        );
        assert_eq!(
            join("node", &["a b.js".into(), String::new()]),
            "node 'a b.js' ''"
        );
        // Every word of up to five of the characters the rules treat
        // specially comes back as it was.
        let alphabet = ['a', ' ', '\'', '"', '\\', '\n'];
        let mut all: Vec<String> = vec![String::new()];
        let mut last = all.clone();
        for _ in 0..5 {
            last = last
                .iter()
                .flat_map(|word| {
                    alphabet.iter().map(move |character| {
                        let mut longer = word.clone();
                        longer.push(*character);
                        longer
                    })
                })
                .collect();
            all.extend(last.iter().cloned());
        }
        for word in &all {
            let arguments = [word.clone(), "end".to_owned()];
            let line = join("tool", &arguments);
            assert_eq!(
                words(&line),
                ["tool", word.as_str(), "end"],
                "{word:?} was written as {line:?}"
            );
        }
    }
}
