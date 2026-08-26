//! chrony.conf-compatible configuration (the documented subset).
//!
//! Unknown directives are collected, never fatal: `config.migrate` honesty means
//! reporting what we dropped, not silently eating it.

use core::fmt;

#[derive(Clone, Debug, PartialEq)]
pub struct ServerDirective {
    pub host: String,
    pub is_pool: bool,
    pub iburst: bool,
    pub min_poll: Option<i8>,
    pub max_poll: Option<i8>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    pub servers: Vec<ServerDirective>,
    /// (threshold seconds, update limit) — chrony `makestep`.
    pub makestep: Option<(f64, u32)>,
    /// chrony `maxslewrate`, ppm.
    pub max_slew_ppm: Option<f64>,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    /// Directives we recognized as chrony's but do not implement yet, with line
    /// numbers (1-based).
    pub ignored: Vec<(usize, String)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConfigError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "config line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ConfigError {}

pub fn parse(text: &str) -> Result<Config, ConfigError> {
    let mut cfg = Config::default();
    for (idx, raw) in text.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw.trim();
        // chrony comment characters: '!', ';', '#', '%'.
        if line.is_empty() || matches!(line.as_bytes()[0], b'!' | b';' | b'#' | b'%') {
            continue;
        }
        let mut words = line.split_whitespace();
        let Some(directive) = words.next() else {
            continue;
        };
        let args: Vec<&str> = words.collect();
        match directive.to_ascii_lowercase().as_str() {
            d @ ("server" | "pool") => {
                let host = args
                    .first()
                    .ok_or_else(|| err(line_no, format!("{d} needs a host")))?;
                let mut s = ServerDirective {
                    host: (*host).to_string(),
                    is_pool: d == "pool",
                    iburst: false,
                    min_poll: None,
                    max_poll: None,
                };
                let mut rest = args[1..].iter();
                while let Some(opt) = rest.next() {
                    match opt.to_ascii_lowercase().as_str() {
                        "iburst" => s.iburst = true,
                        "minpoll" => {
                            s.min_poll = Some(parse_num(line_no, "minpoll", rest.next().copied())?)
                        }
                        "maxpoll" => {
                            s.max_poll = Some(parse_num(line_no, "maxpoll", rest.next().copied())?)
                        }
                        other => {
                            cfg.ignored.push((line_no, format!("{d} option '{other}'")));
                            // Skip a value-taking option's argument when known.
                            if matches!(other, "key" | "maxdelay" | "maxdelayratio" | "presend") {
                                let _ = rest.next();
                            }
                        }
                    }
                }
                cfg.servers.push(s);
            }
            "makestep" => {
                let threshold: f64 =
                    parse_num(line_no, "makestep threshold", args.first().copied())?;
                let limit: i64 = parse_num(line_no, "makestep limit", args.get(1).copied())?;
                // chrony: negative limit = always allowed.
                let limit = if limit < 0 { u32::MAX } else { limit as u32 };
                cfg.makestep = Some((threshold, limit));
            }
            "maxslewrate" => {
                cfg.max_slew_ppm = Some(parse_num(line_no, "maxslewrate", args.first().copied())?);
            }
            "allow" => cfg
                .allow
                .push(args.first().copied().unwrap_or("all").to_string()),
            "deny" => cfg
                .deny
                .push(args.first().copied().unwrap_or("all").to_string()),
            other => {
                cfg.ignored.push((line_no, other.to_string()));
            }
        }
    }
    Ok(cfg)
}

fn err(line: usize, message: String) -> ConfigError {
    ConfigError { line, message }
}

fn parse_num<T: core::str::FromStr>(
    line: usize,
    what: &str,
    value: Option<&str>,
) -> Result<T, ConfigError> {
    let v = value.ok_or_else(|| err(line, format!("{what} needs a value")))?;
    v.parse()
        .map_err(|_| err(line, format!("{what}: cannot parse '{v}'")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typical_chrony_conf() {
        let text = "\
# NTP servers
pool 2.pool.ntp.org iburst
server ntp.example.com iburst minpoll 4 maxpoll 8
makestep 1.0 3
maxslewrate 83333.0
driftfile /var/lib/chrony/drift
rtcsync
allow 192.168.0.0/16
";
        let cfg = parse(text).expect("parse");
        assert_eq!(cfg.servers.len(), 2);
        assert!(cfg.servers[0].is_pool && cfg.servers[0].iburst);
        assert_eq!(cfg.servers[1].min_poll, Some(4));
        assert_eq!(cfg.makestep, Some((1.0, 3)));
        assert_eq!(cfg.max_slew_ppm, Some(83333.0));
        assert_eq!(cfg.allow, vec!["192.168.0.0/16".to_string()]);
        // driftfile + rtcsync recognized as dropped, with line numbers.
        assert_eq!(cfg.ignored.len(), 2);
        assert_eq!(cfg.ignored[0], (6, "driftfile".to_string()));
    }

    #[test]
    fn negative_makestep_limit_means_always() {
        let cfg = parse("makestep 0.1 -1").expect("parse");
        assert_eq!(cfg.makestep, Some((0.1, u32::MAX)));
    }

    #[test]
    fn bad_value_is_an_error_with_line() {
        let e = parse("server\n").expect_err("should fail");
        assert_eq!(e.line, 1);
        let e = parse("# ok\nmakestep abc 3").expect_err("should fail");
        assert_eq!(e.line, 2);
    }
}
