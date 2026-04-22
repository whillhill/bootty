use anyhow::{Context, Result};
use rand::Rng;
use std::time::Duration;
use tokio::time::sleep;

const TEN_KB_UP_URL: &str = "https://up.10kb.site/";
const TEN_KB_URL: &str = "https://www.10kb.site/";

pub async fn create_10kb_file(path: &str, body: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}{}", TEN_KB_UP_URL, path))
        .body(body.to_string())
        .send()
        .await
        .context("10kb.site upload failed")?;

    if resp.status() != reqwest::StatusCode::CREATED {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("10kb.site error: {}", body);
    }
    Ok(())
}

pub async fn read_10kb_file(path: &str) -> Result<(reqwest::StatusCode, String)> {
    let resp = reqwest::get(format!("{}{}", TEN_KB_URL, path))
        .await
        .context("10kb.site read failed")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    Ok((status, body))
}

pub async fn poll_for_response(path: &str) -> Result<String> {
    loop {
        let (status, body) = read_10kb_file(path).await?;
        if status == reqwest::StatusCode::OK {
            return Ok(body);
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            sleep(Duration::from_millis(300)).await;
            continue;
        }
        anyhow::bail!("Unexpected 10kb.site status: {}", status);
    }
}

pub fn rand_seq(n: usize) -> String {
    let chars: Vec<char> =
        "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ"
            .chars()
            .collect();
    let mut rng = rand::thread_rng();
    (0..n)
        .map(|_| chars[rng.gen_range(0..chars.len())])
        .collect()
}
