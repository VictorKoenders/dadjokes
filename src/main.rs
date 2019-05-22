use failure::{bail, ResultExt};
use lazy_static::lazy_static;
use log::{error, info};
use rand::seq::SliceRandom;
use rodio::source::Source;
use rusoto_core::{HttpClient, Region};
use rusoto_credential::StaticProvider;
use rusoto_polly::{Polly, PollyClient};
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader, Cursor, Write};

lazy_static! {
    static ref SPACESTATE_URL: String =
        std::env::var("SPACESTATE_URL").expect("Missing environment variable SPACESTATE_URL");
    static ref REDDIT_URL: String =
        std::env::var("REDDIT_URL").expect("Missing environment variable REDDIT_URL");
    static ref USED_IDS_FILE: String =
        std::env::var("USED_IDS_FILE").expect("Missing environment variable USED_IDS_FILE");
    static ref AWS_POLLY_ACCESS_KEY: String = std::env::var("AWS_POLLY_ACCESS_KEY")
        .expect("Missing environment variable AWS_POLLY_ACCESS_KEY");
    static ref AWS_POLLY_SECRET_ACCESS_KEY: String = std::env::var("AWS_POLLY_SECRET_ACCESS_KEY")
        .expect("Missing environment variable AWS_POLLY_SECRET_ACCESS_KEY");
}

fn main() {
    dotenv::dotenv().expect("Could not read .env, have you copied .env.example?");

    env_logger::init();
    let cursor = crossterm::cursor();
    let terminal = crossterm::terminal();
    cursor.hide().expect("Could not hide cursor");
    terminal
        .clear(crossterm::ClearType::All)
        .expect("Could not clear terminal");

    let mut broadcasted_dadjokes = load_used_ids().unwrap_or_default();
    let client = PollyClient::new_with(
        HttpClient::new().expect("Could not make http client"),
        StaticProvider::new_minimal(
            AWS_POLLY_ACCESS_KEY.to_owned(),
            AWS_POLLY_SECRET_ACCESS_KEY.to_owned(),
        ),
        Region::EuWest1,
    );

    let voices = client
        .describe_voices(rusoto_polly::DescribeVoicesInput {
            language_code: Some(String::from("en-US")),
            ..Default::default()
        })
        .sync()
        .expect("Could not describe voices");
    let voices = voices.voices.unwrap();
    let mut rand = rand::thread_rng();
    let device = rodio::default_output_device().expect("Could not find default audio device");
    info!("Playing audio on {:?}", device.name());

    loop {
        let voice = voices.choose(&mut rand).unwrap();
        if let Err(e) = run(
            &mut broadcasted_dadjokes,
            &client,
            &device,
            &cursor,
            &terminal,
            voice,
        ) {
            error!("Could not generate pun: {:?}", e);
        }
        std::thread::sleep(std::time::Duration::from_secs(30));
    }
}
fn run(
    used_jokes: &mut Vec<String>,
    client: &PollyClient,
    device: &rodio::Device,
    cursor: &crossterm::TerminalCursor,
    terminal: &crossterm::Terminal,
    voice: &rusoto_polly::Voice,
) -> Result<(), failure::Error> {
    if !space_is_open().context("Could not get spacestate")? {
        info!("Space is not open");
        return Ok(());
    }
    let posts = load_newest_reddit_posts().context("Could not load reddit posts")?;
    let highest = match posts.iter().max_by_key(|p| p.score) {
        Some(post) => post,
        None => bail!("Did not find a single post"),
    };
    if used_jokes.contains(&highest.id) {
        info!("Ignoring joke that has already been told: {:?}", highest);
        return Ok(());
    }
    info!("{:#?}", highest);
    used_jokes.push(highest.id.clone());
    let mut output =
        File::create(&*USED_IDS_FILE).context("Could not open USED_IDS_FILE for writing")?;
    for id in used_jokes {
        writeln!(&mut output, "{}", id).context("Could not save USER_IDS_FILE")?;
    }

    let result = client
        .synthesize_speech(rusoto_polly::SynthesizeSpeechInput {
            output_format: String::from("mp3"),
            text: format!("{}\n\n{}", highest.title, highest.selftext),
            voice_id: voice.id.clone().unwrap(),
            ..Default::default()
        })
        .sync()
        .context("Could not synthesize speech")?;
    let stream = result.audio_stream.unwrap();
    let decoder = rodio::Decoder::new(Cursor::new(stream)).context("Could not create decoder")?;
    rodio::play_raw(device, decoder.convert_samples());

    let (width, height) = terminal.terminal_size();
    terminal
        .clear(crossterm::ClearType::All)
        .context("Could not clear screen")?;

    {
        // TODO What if this is wider than the terminal?
        let x = (width - highest.title.len() as u16) / 2;
        let y = height / 2 - 1;
        cursor.goto(x, y).context("Could not move cursor")?;
        terminal
            .write(&highest.title)
            .context("Could not write title")?;
    }
    {
        // TODO What if this is wider than the terminal?
        let mut y = height / 2 + 1;
        for line in highest.selftext.split('\n') {
            let x = (width - line.len() as u16) / 2;
            cursor.goto(x, y).context("Could not move cursor")?;
            terminal.write(line).context("Could not write selftext")?;
            y += 1;
        }
    }

    Ok(())
}

fn space_is_open() -> Result<bool, reqwest::Error> {
    let mut response = reqwest::get("https://spacestate.pixelbar.nl/spacestate.php")?;
    let response: Value = response.json()?;
    Ok(if let Some(Value::String(s)) = response.get("state") {
        s == "open"
    } else {
        false
    })
}

fn load_newest_reddit_posts() -> Result<Vec<RedditPost>, reqwest::Error> {
    let mut response = reqwest::get(&*REDDIT_URL)?;
    let json: Value = response.json()?;
    let mut result = Vec::new();

    if let Some(Value::Array(a)) = json.pointer("/data/children") {
        for child in a {
            let id = child.pointer("/data/id");
            let title = child.pointer("/data/title");
            let selftext = child.pointer("/data/selftext");
            let score = child.pointer("/data/score");

            if let (
                Some(Value::String(id)),
                Some(Value::String(title)),
                Some(Value::String(selftext)),
                Some(Value::Number(score)),
            ) = (id, title, selftext, score)
            {
                result.push(RedditPost {
                    id: id.to_owned(),
                    title: title.to_owned(),
                    selftext: selftext.to_owned(),
                    score: score.as_i64().unwrap_or_default(),
                });
            } else {
                error!("Missing values of {:?}", child);
                error!("id: {:?}", id);
                error!("title: {:?}", title);
                error!("selftext: {:?}", selftext);
                error!("score: {:?}", score);
            }
        }
    }

    Ok(result)
}

#[derive(Debug)]
struct RedditPost {
    id: String,
    title: String,
    selftext: String,
    score: i64,
}

fn load_used_ids() -> std::io::Result<Vec<String>> {
    let file = File::open(&*USED_IDS_FILE)?;
    let lines = BufReader::new(file)
        .lines()
        .filter_map(|l| l.ok())
        .filter(|l| !l.is_empty())
        .collect();
    Ok(lines)
}
