use std::cmp::{max, min};
use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Result};
use chrono::prelude::*;
use dht22_pi::{read, Reading};
use plotters::prelude::*;
use rand::prelude::*;
use rppal::gpio::Gpio;
use rusqlite::{params, Connection};
use serde::Deserialize;
use tiny_http::{Header, Response, Server, StatusCode};
use url::Url;

fn read_sensor(pin: u8, delay: Duration) -> Result<Reading> {
    let mut i = 0;
    loop {
        match read(pin) {
            Ok(r) => return Ok(r),
            Err(err) => {
                if i > 10 {
                    return Err(anyhow!("could not read pin {}: {:?}", pin, err));
                }
                println!("{:?}: sleeping...", err);
                sleep(delay);
                i += 1;
            }
        }
    }
}

fn record_sensors(conn: Arc<Mutex<Connection>>, config: &Config) {
    let wait = config.sensor_read();
    let mut first = true;

    loop {
        for (name, sensor) in &config.sensors {
            let mut reading = match read_sensor(sensor.pin, config.retry_read()) {
                Ok(r) => r,
                Err(err) => {
                    println!("{}, skipping", err);
                    continue;
                }
            };
            reading.temperature = c_to_f(reading.temperature);
            if first {
                continue;
            }
            if let Err(err) = record_reading(&conn, name, &reading) {
                println!("could not record in db: {}", err);
            }
            println!("checking {} actions", name);
            for action in &sensor.actions {
                let trigger = match action.typ.as_str() {
                    "temp below" => reading.temperature < action.value,
                    "temp above" => reading.temperature > action.value,
                    _ => panic!("unknown typ {}", action.typ),
                };
                if !trigger {
                    continue;
                }
                let mut pin = Gpio::new()
                    .expect("could not get gpio")
                    .get(action.pin)
                    .expect("could not get pin")
                    .into_output();
                match action.action.as_str() {
                    "enable" => pin.set_high(),
                    "disable" => pin.set_low(),
                    _ => panic!("unknown action {}", action.action),
                };
                println!(
                    "{} pin {} because {} {} {}",
                    action.action, action.pin, name, action.typ, action.value
                );
            }
        }
        // Ignore first read because it seemed off one time.
        if first {
            first = false;
            continue;
        }
        println!("waiting {:?}", wait);
        sleep(wait);
    }
}

fn record_reading(conn: &Arc<Mutex<Connection>>, name: &str, r: &Reading) -> Result<()> {
    let conn = conn.lock().unwrap();
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    conn.execute(
        "INSERT INTO readings VALUES (?, ?, ?), (?, ?, ?)",
        params![
            format!("temp-{}", name),
            now,
            r.temperature as f64,
            format!("humidity-{}", name),
            now,
            r.humidity as f64,
        ],
    )?;
    Ok(())
}

fn c_to_f(c: f32) -> f32 {
    c * 1.8 + 32.0
}

#[derive(Deserialize, Debug)]
struct Config {
    sensor_read_freq_secs: u64,
    retry_read_secs: u64,
    sensors: HashMap<String, Sensor>,
}

impl Config {
    fn sensor_read(&self) -> Duration {
        Duration::from_secs(self.sensor_read_freq_secs)
    }
    fn retry_read(&self) -> Duration {
        Duration::from_secs(self.retry_read_secs)
    }
}

#[derive(Deserialize, Debug)]
struct Sensor {
    pin: u8,
    actions: Vec<Action>,
}

#[derive(Deserialize, Debug)]
struct Action {
    typ: String,
    value: f32,
    action: String,
    pin: u8,
}

fn main() -> Result<()> {
    let config = std::fs::read("config.toml").expect("could not read config.toml");
    let config: Config = toml::from_slice(&config).expect("could not parse config.toml");
    println!("{:?}", config);

    let conn = init_db().unwrap();

    let port: u16 = std::env::var("PORT")
        .unwrap_or("3000".to_string())
        .parse()
        .unwrap();
    println!("listening on http://127.0.0.1:{}/", port);
    let server = Server::http(format!("0.0.0.0:{}", port)).unwrap();

    let server = Arc::new(server);
    let mut guards = Vec::with_capacity(4);
    let conn = Arc::new(Mutex::new(conn));

    let record_conn = Arc::clone(&conn);
    std::thread::spawn(move || {
        record_sensors(record_conn, &config);
    });

    for _ in 0..guards.capacity() {
        let server = server.clone();
        let thread_conn = Arc::clone(&conn);

        let guard = std::thread::spawn(move || loop {
            let req = server.recv().unwrap();
            let url = format!("http://{}{}", req.remote_addr(), req.url());
            println!("req: {}", url);
            let url = match Url::parse(&url) {
                Ok(url) => url,
                Err(err) => {
                    println!("{}", err);
                    continue;
                }
            };
            let req_conn = Arc::clone(&thread_conn);
            let resp = match url.path() {
                "/" => index(),
                "/render" => render(req_conn, url.query_pairs()),
                p @ _ => {
                    Ok(Response::from_string(format!("unknown path: {}", p)).with_status_code(404))
                }
            };
            let ok = req.respond(match resp {
                Ok(resp) => resp,
                Err(err) => {
                    println!("error: {}", err);
                    Response::from_string(format!("{:?}", err)).with_status_code(500)
                }
            });
            if ok.is_err() {
                println!("respond error: {:?}", ok.unwrap_err());
            }
        });

        guards.push(guard);
    }

    for t in guards {
        t.join().unwrap();
    }

    Ok(())
}

fn html_response<D: Into<Vec<u8>>>(data: D) -> Response<Cursor<Vec<u8>>> {
    let data = data.into();
    let data_len = data.len();

    Response::new(
        StatusCode(200),
        vec![Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=UTF-8"[..]).unwrap()],
        Cursor::new(data),
        Some(data_len),
        None,
    )
}

fn index() -> Result<Response<Cursor<Vec<u8>>>> {
    Ok(html_response(INDEX))
}

fn render(
    conn: Arc<Mutex<Connection>>,
    query: url::form_urlencoded::Parse<'_>,
) -> Result<Response<Cursor<Vec<u8>>>> {
    let mut names = vec![];
    let mut xmax = None;
    let mut xmin = None;
    let mut title = None;
    for (key, val) in query {
        match key.to_string().as_str() {
            "name" => names.push(val),
            "xmin" => xmin = Some(val.parse::<f64>()?),
            "xmax" => xmax = Some(val.parse::<f64>()?),
            "title" => title = Some(val),
            _ => bail!("unknown render key {}", key),
        }
    }

    let conn = conn.lock().unwrap();
    let mut ts_min = Utc::now();
    let mut ts_max = ts_min
        .checked_sub_signed(chrono::Duration::weeks(1))
        .unwrap();
    let mut val_min = 200.0;
    let mut val_max = -200.0;
    let mut series = HashMap::new();

    for name in names {
        let mut stmt = conn.prepare("SELECT ts, value FROM readings WHERE name = ?")?;
        let mut rows = stmt.query(params![name])?;

        let mut readings: Vec<(DateTime<Utc>, f64)> = vec![];
        while let Some(row) = rows.next()? {
            let ts = Utc.timestamp(row.get(0)?, 0);
            ts_min = min(ts_min, ts);
            ts_max = max(ts_max, ts);
            let val: f64 = row.get(1)?;
            if val < val_min {
                val_min = val;
            }
            if val > val_max {
                val_max = val;
            }
            readings.push((ts, val));
        }
        if readings.is_empty() || ts_min == ts_max {
            return Err(anyhow!("no data"));
        }
        if val_min == val_max {
            val_min -= 10.0;
            val_max += 10.0;
        }
        series.insert(name, readings);
    }

    if let Some(xmax) = xmax {
        val_max = xmax;
    }
    if let Some(xmin) = xmin {
        val_min = xmin;
    }
    let title = match title {
        Some(title) => title,
        None => bail!("no title"),
    };

    let mut data = String::with_capacity(1024);
    {
        let root = SVGBackend::with_string(&mut data, (640, 480)).into_drawing_area();
        root.fill(&WHITE)?;
        let mut chart = ChartBuilder::on(&root)
            .caption(title, ("sans-serif", 30).into_font())
            .margin(5)
            .x_label_area_size(30)
            .y_label_area_size(30)
            .build_cartesian_2d(ts_min..ts_max, val_min..val_max)?;

        chart
            .configure_mesh()
            .x_label_formatter(&|d| d.format("%a %R").to_string())
            .draw()?;

        let mut i = 0;
        for (name, data) in series {
            let color = &COLORS[i % COLORS.len()];
            i += 1;
            chart
                .draw_series(LineSeries::new(data, color))?
                .label(name)
                .legend(move |(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], color));
        }
        chart
            .configure_series_labels()
            .position(SeriesLabelPosition::UpperLeft)
            .border_style(&BLACK)
            .draw()?;
    }

    Ok(Response::from_data(data).with_header(
        tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"image/svg+xml"[..]).unwrap(),
    ))
}

static COLORS: [RGBColor; 2] = [RGBColor(114, 165, 83), RGBColor(202, 85, 114)];

fn init_db() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    create_db(&conn)?;
    //sample_data(&conn)?;
    Ok(conn)
}

fn create_db(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS readings (
          name  STRING NOT NULL,
          ts    INT8, -- unix epoch seconds
          value FLOAT8,
          PRIMARY KEY (name, ts)
        );",
        params![],
    )?;
    Ok(())
}

#[allow(dead_code)]
fn sample_data(conn: &Connection) -> Result<()> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let mut rng = rand::thread_rng();
    let mut t1: f64 = rng.gen_range(30.0, 70.0);
    let mut t2: f64 = rng.gen_range(10.0, 90.0);
    for i in 0..1000 {
        t1 = next(&mut rng, t1, 2.0, 30.0, 70.0);
        t2 = next(&mut rng, t2, 4.0, 10.0, 90.0);
        conn.execute(
            r#"INSERT INTO readings VALUES ("temp-inside", ?, ?);"#,
            params![now - (i * 60 * 5), t1],
        )?;
        conn.execute(
            r#"INSERT INTO readings VALUES ("humidity-inside", ?, ?);"#,
            params![now - (i * 60 * 5), t2],
        )?;
    }
    Ok(())
}

fn next(rng: &mut ThreadRng, f: f64, step: f64, min: f64, max: f64) -> f64 {
    let mut f = f + rng.gen_range(-step, step);
    if f < min {
        f = min;
    }
    if f > max {
        f = max;
    }
    f
}

const INDEX: &str = r#"
<!DOCTYPE html>
<html lang="en-us">
	<head>
		<meta http-equiv="content-type" content="text/html; charset=utf-8" />
		<title>cheese cave control</title>
		<style>
			:root {
				--primary: #6200ee;
				--variant: #3700b3;
				--secondary: #03dac6;
				--secondary-variant: #018786;
				--background: #ffffff;
				--surface: #ffffff;
				--error: #b00020;
				--on-primary: #ffffff;
				--on-secondary: #000000;
				--on-background: #000000;
				--on-surface: #000000;
				--on-error: #ffffff;
				--dp00: #ffffff;
				--dp01: #f2f2f2;
				--dp02: #ededed;
				--dp03: #ebebeb;
				--dp04: #e8e8e8;
				--dp06: #e3e3e3;
				--dp08: #e0e0e0;
				--dp12: #dbdbdb;
				--dp16: #d9d9d9;
				--dp24: #d6d6d6;
				--emph-high: #212121;
				--emph-medium: #666666;
				--disabled: #9e9e9e;
			}
			@media (prefers-color-scheme: dark) {
				:root {
					--primary: #bb86fc;
					--variant: #3700b3;
					--secondary: #03dac6;
					--secondary-variant: #03dac6;
					--background: #121212;
					--surface: #121212;
					--error: #cf6679;
					--on-primary: #000000;
					--on-secondary: #000000;
					--on-background: #ffffff;
					--on-surface: #ffffff;
					--on-error: #000000;
					--dp00: #121212;
					--dp01: #1e1e1e;
					--dp02: #232323;
					--dp03: #252525;
					--dp04: #272727;
					--dp06: #2c2c2c;
					--dp08: #2e2e2e;
					--dp12: #333333;
					--dp16: #363636;
					--dp24: #383838;
					--emph-high: #e0e0e0;
					--emph-medium: #a0a0a0;
					--disabled: #6c6c6c;
				}
			}
			body {
				color: var(--emph-high);
				background-color: var(--background);
			}
		</style>
		<style>
			body {
				max-width: 38rem;
				padding-left: 1rem;
				padding-right: 1rem;
				margin-left: auto;
				margin-right: auto;
				font-size: 20px;
				font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Helvetica,
					Arial, sans-serif;
			}
			.link-title a {
				color: var(--emph-high);
			}
			small {
				color: var(--emph-medium);
			}
			a {
				color: var(--primary);
				text-decoration: none;
			}
			a:hover {
				text-decoration: underline;
			}
			pre,
			code {
				tab-size: 2;
				background-color: var(--dp03);
				font-size: 1rem;
			}
			pre code {
				background-color: transparent;
			}
			.title {
				border-bottom: 1px solid var(--disabled);
			}
			.title a {
				text-decoration: none;
			}
			.blog-title {
				margin-bottom: 10px;
			}
			blockquote {
				padding: 1rem;
				background: var(--dp01);
			}
			blockquote p {
				margin: 0;
			}
			.img {
				max-width: 100%;
			}
		</style>
	</head>
	<body>
		<h3 class="title link-title">
			<a href="/">
				cheese cave control
			</a>
		</h3>
		<div>
			<img src="/render?name=temp-inside&name=humidity-inside&xmin=0&xmax=100&title=inside" alt="inside" class="img" />
		</div>
	</body>
</html>
"#;
