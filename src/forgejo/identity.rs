//! Explicit cross-provider presentation identities. These mappings never grant
//! permissions, rewrite Git authors, or turn forwarded comments into commands.
use std::{collections::BTreeMap, ops::Range, sync::OnceLock};

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

use super::bridge::Target;
use crate::{config::Config, github::enc_seg};

#[derive(Debug, Default)]
pub(crate) struct UserMap {
    github: BTreeMap<String, String>,
    forgejo: BTreeMap<String, String>,
}

impl<'de> Deserialize<'de> for UserMap {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UniqueUsers;
        impl<'de> Visitor<'de> for UniqueUsers {
            type Value = UserMap;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a one-to-one object of Forgejo login to GitHub login")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<UserMap, M::Error> {
                let mut users = UserMap::default();
                while let Some((fj, gh)) = map.next_entry::<String, String>()? {
                    let valid_fj = !fj.is_empty()
                        && fj.len() <= 100
                        && fj
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
                        && fj.bytes().next().is_some_and(|c| c.is_ascii_alphanumeric());
                    let valid_gh =
                        crate::commands::is_valid_login(gh.strip_suffix("[bot]").unwrap_or(&gh));
                    if !valid_fj || !valid_gh {
                        return Err(serde::de::Error::custom(
                            "use account logins, without @, URLs or display names",
                        ));
                    }
                    if users
                        .github
                        .insert(fj.to_ascii_lowercase(), gh.clone())
                        .is_some()
                        || users.forgejo.insert(gh.to_ascii_lowercase(), fj).is_some()
                    {
                        return Err(serde::de::Error::custom(
                            "user mappings must be one-to-one, ignoring case",
                        ));
                    }
                }
                Ok(users)
            }
        }
        deserializer.deserialize_map(UniqueUsers)
    }
}

impl UserMap {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        serde_json::from_str(value).map_err(|e| format!("invalid FORGEJO_USER_MAP: {e}"))
    }
}

pub(crate) struct Identities {
    users: UserMap,
    forgejo_base: String,
    forgejo_bot: String,
    github_bot: String,
    github_alias: String,
}

impl Identities {
    pub(crate) fn new(cfg: &Config, app_slug: &str) -> Result<Self, String> {
        let base = cfg.forgejo_base_url.trim_end_matches('/');
        let slug = crate::github::normalize_login(app_slug);
        Ok(Self {
            users: UserMap::parse(&cfg.forgejo_user_map)?,
            forgejo_base: base.strip_suffix("/api/v1").unwrap_or(base).into(),
            forgejo_bot: cfg.forgejo_bot_name.clone(),
            github_bot: if slug.is_empty() {
                String::new()
            } else {
                format!("{slug}[bot]")
            },
            github_alias: cfg.bot_name.clone(),
        })
    }

    fn mapped(&self, login: &str, target: Target) -> Option<&str> {
        match target {
            Target::GitHub => {
                if !self.github_bot.is_empty() && login.eq_ignore_ascii_case(&self.forgejo_bot) {
                    Some(&self.github_bot)
                } else {
                    self.users
                        .github
                        .get(&login.to_ascii_lowercase())
                        .map(String::as_str)
                }
            }
            Target::Forgejo => {
                if !self.forgejo_bot.is_empty()
                    && !self.github_bot.is_empty()
                    && (login.eq_ignore_ascii_case(&self.github_bot)
                        || login.eq_ignore_ascii_case(self.github_bot.trim_end_matches("[bot]"))
                        || login.eq_ignore_ascii_case(&self.github_alias))
                {
                    Some(&self.forgejo_bot)
                } else {
                    self.users
                        .forgejo
                        .get(&login.to_ascii_lowercase())
                        .map(String::as_str)
                }
            }
        }
    }

    fn profile(&self, login: &str, provider: Target) -> String {
        let base = match provider {
            Target::GitHub => "https://github.com",
            Target::Forgejo => &self.forgejo_base,
        };
        let label = login
            .replace('\\', "\\\\")
            .replace('[', "\\[")
            .replace(']', "\\]")
            .replace('*', "\\*")
            .replace('_', "\\_")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('`', "\\`");
        format!("[@{label}]({base}/{})", enc_seg(login))
    }

    pub(crate) fn author(&self, login: &str, target: Target) -> String {
        let (source, name) = match target {
            Target::GitHub => (Target::Forgejo, "Forgejo"),
            Target::Forgejo => (Target::GitHub, "GitHub"),
        };
        let original = self.profile(login, source);
        if let Some(mapped) = self.mapped(login, target) {
            format!("**{}** ({name}: {original})", self.profile(mapped, target))
        } else {
            format!("**{original}** ({name})")
        }
    }

    /// Only edit Markdown prose ranges. Keep code, explicit links, HTML,
    /// escapes, emails and URLs byte-for-byte; never reserialize Markdown.
    pub(crate) fn markdown(&self, body: &str, target: Target) -> String {
        let mut prose: Vec<Range<usize>> = Vec::new();
        let mut protected = 0usize;
        let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_MATH;
        for (event, range) in Parser::new_ext(body, options).into_offset_iter() {
            match event {
                Event::Start(
                    Tag::CodeBlock(_) | Tag::Link { .. } | Tag::Image { .. } | Tag::HtmlBlock,
                ) => protected += 1,
                Event::End(
                    TagEnd::CodeBlock | TagEnd::Link | TagEnd::Image | TagEnd::HtmlBlock,
                ) => protected = protected.saturating_sub(1),
                Event::Text(_) if protected == 0 => {
                    if let Some(previous) = prose.last_mut().filter(|r| r.end == range.start) {
                        previous.end = range.end;
                    } else {
                        prose.push(range);
                    }
                }
                _ => (),
            }
        }
        static MENTIONS: OnceLock<regex::Regex> = OnceLock::new();
        let regex = MENTIONS.get_or_init(|| {
            regex::Regex::new(
                r"@[A-Za-z0-9][A-Za-z0-9._-]*(?:\[bot\])?(?:/[A-Za-z0-9][A-Za-z0-9_-]*)?",
            )
            .unwrap()
        });
        let mut result = String::with_capacity(body.len());
        let mut previous = 0;
        for m in regex.find_iter(body) {
            let raw = m.as_str().trim_end_matches('.');
            let end = m.start() + raw.len();
            if !prose.iter().any(|r| r.start <= m.start() && end <= r.end) {
                continue;
            }
            let before = &body[..m.start()];
            if before
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || "_/@\\".contains(c))
                || before
                    .split_whitespace()
                    .next_back()
                    .is_some_and(|word| word.contains("://"))
            {
                continue;
            }
            let login = &raw[1..];
            let replacement = if let Some(mapped) = self.mapped(login, target) {
                format!("@{mapped}")
            } else if login.contains('/') {
                // Teams have provider-specific semantics; do not notify a
                // same-named team on the destination without a binding.
                format!("`{raw}`")
            } else {
                self.profile(
                    login,
                    match target {
                        Target::GitHub => Target::Forgejo,
                        Target::Forgejo => Target::GitHub,
                    },
                )
            };
            result.push_str(&body[previous..m.start()]);
            result.push_str(&replacement);
            previous = end;
        }
        result.push_str(&body[previous..]);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identities() -> Identities {
        let mut cfg = Config::from_env();
        cfg.forgejo_base_url = "https://code.example/forgejo/api/v1/".into();
        cfg.forgejo_bot_name = "zero-bot".into();
        cfg.bot_name = "xero-alias".into();
        cfg.forgejo_user_map = r#"{"thefool":"YUZHEthefool","new_user":"new-github"}"#.into();
        Identities::new(&cfg, "xero-team-bot").unwrap()
    }
    #[test]
    fn user_mapping_is_bidirectional_and_does_not_guess_same_names() {
        let i = identities();
        assert_eq!(
            i.markdown("Hi @Thefool, @new_user and @stranger.", Target::GitHub),
            "Hi @YUZHEthefool, @new-github and [@stranger](https://code.example/forgejo/stranger)."
        );
        assert_eq!(
            i.markdown("@yuzhethefool @new-github", Target::Forgejo),
            "@thefool @new_user"
        );
        assert_eq!(i.author("thefool", Target::GitHub), "**[@YUZHEthefool](https://github.com/YUZHEthefool)** (Forgejo: [@thefool](https://code.example/forgejo/thefool))");
    }
    #[test]
    fn bots_use_provider_logins_instead_of_display_names() {
        let i = identities();
        assert_eq!(
            i.markdown("@zero-bot try", Target::GitHub),
            "@xero-team-bot[bot] try"
        );
        assert_eq!(
            i.markdown(
                "@xero-team-bot[bot] @xero-team-bot @xero-alias",
                Target::Forgejo
            ),
            "@zero-bot @zero-bot @zero-bot"
        );
    }
    #[test]
    fn markdown_code_links_escapes_urls_and_emails_are_preserved() {
        let i = identities();
        let protected = "`@thefool`\n\n```rust\n@thefool\n```\n\n    @thefool\n\n[@thefool](https://example.org/@thefool) ![@thefool](img.png) <https://example.org/@thefool> a@thefool.chat https://example.org/@thefool \\@thefool\n<!-- @thefool -->\n";
        assert_eq!(i.markdown(protected, Target::GitHub), protected);
        assert_eq!(
            i.markdown("中文 **@thefool** and (@thefool).", Target::GitHub),
            "中文 **@YUZHEthefool** and (@YUZHEthefool)."
        );
    }
    #[test]
    fn mappings_reject_ambiguous_or_invalid_configuration() {
        for value in [
            r#"{"a":"x","A":"y"}"#,
            r#"{"a":"x","b":"X"}"#,
            r#"{"a":"x","a":"y"}"#,
            r#"{"@a":"x"}"#,
            r#"{"a":"Display Name"}"#,
            r#"{"a":"https://github.com/x"}"#,
            "[]",
            "null",
            "{bad",
        ] {
            assert!(UserMap::parse(value).is_err(), "{value}");
        }
    }
}
