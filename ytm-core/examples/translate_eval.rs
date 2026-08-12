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
//! `ai` is empty on every line when no key is set, which is also a fair way to
//! check that the free path still works on its own.

use ytm_core::translate::{Ai, Backend, translate_lines};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let to = std::env::args().nth(1).unwrap_or_else(|| "zh".to_string());
    let model = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "claude-haiku-4-5".to_string());
    let lines: Vec<String> = std::io::read_to_string(std::io::stdin())?
        .lines()
        .map(str::to_string)
        .collect();

    let free = translate_lines(&lines, &Backend::free(&to)).await?;

    let ai = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(api_key) if !api_key.trim().is_empty() => {
            let backend = Backend {
                to: to.clone(),
                ai: Some(Ai { model, api_key }),
            };
            translate_lines(&lines, &backend).await?
        }
        _ => {
            eprintln!("no ANTHROPIC_API_KEY — free path only");
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
