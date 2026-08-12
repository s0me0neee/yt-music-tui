//! Compares the two translation backends on the same lyrics.
//!
//! Reads lyric lines from stdin and translates them twice — once through the
//! free path and once through the AI path — writing one JSON record per line so
//! the two can be diffed, or scored against a reference translation:
//!
//! ```text
//! ANTHROPIC_API_KEY=… cargo run -p ytm-core --example translate_eval -- zh \
//!     < lines.txt > pairs.jsonl
//! ```
//!
//! `DEEPSEEK_API_KEY` works the same way, and picks that provider.
//!
//! `ai` is empty on every line when no key is set, which is also a fair way to
//! check that the free path still works on its own.

use ytm_core::translate::{Ai, Backend, Provider, translate_lines};

/// The first key set, and whose it is.
fn key() -> Option<(Provider, String)> {
    ["ANTHROPIC_API_KEY", "DEEPSEEK_API_KEY"]
        .into_iter()
        .find_map(|name| {
            let key = std::env::var(name).ok().filter(|k| !k.trim().is_empty())?;
            Some((Provider::for_key_env(name), key))
        })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let to = std::env::args().nth(1).unwrap_or_else(|| "zh".to_string());
    let lines: Vec<String> = std::io::read_to_string(std::io::stdin())?
        .lines()
        .map(str::to_string)
        .collect();

    let free = translate_lines(&lines, &Backend::free(&to)).await?.lines;

    let ai = match key() {
        Some((provider, api_key)) => {
            let model = std::env::args()
                .nth(2)
                .unwrap_or_else(|| provider.default_model().to_string());
            let backend = Backend {
                to: to.clone(),
                ai: Some(Ai {
                    model,
                    api_key,
                    provider,
                }),
            };
            let done = translate_lines(&lines, &backend).await?;
            // Empty when the AI path failed and the free endpoint answered
            // instead — the two columns would otherwise be silently identical.
            if done.model.is_empty() {
                eprintln!("the AI path fell back to the free endpoint — see the log");
            }
            done.lines
        }
        None => {
            eprintln!("no ANTHROPIC_API_KEY or DEEPSEEK_API_KEY — free path only");
            vec![String::new(); lines.len()]
        }
    };

    for (i, line) in lines.iter().enumerate() {
        let record = serde_json::json!({
            "i": i,
            "source": line,
            "free": free[i],
            "ai": ai[i],
        });
        println!("{record}");
    }

    let differing = lines
        .iter()
        .enumerate()
        .filter(|(i, l)| !l.trim().is_empty() && free[*i] != ai[*i])
        .count();
    eprintln!(
        "{} lines, {differing} where the backends differ",
        lines.len()
    );
    Ok(())
}
