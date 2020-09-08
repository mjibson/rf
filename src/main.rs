// Try to cross compile with https://github.com/japaric/rust-cross
// Probably just create a dockerfile that can do it
// cargo build --target=armv7-unknown-linux-gnueabihf

use anyhow::{anyhow, Result};
use chrono::prelude::*;
use dht22_pi::{read, Reading};
use plotters::prelude::*;
use rand::prelude::*;
use rusqlite::{params, Connection};
use tiny_http::{Header, Response, Server, StatusCode};

use std::cmp::{max, min};
use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

fn read_sensor(pin: u8) -> Result<Reading> {
    let mut i = 0;
    loop {
        match read(pin) {
            Ok(r) => return Ok(r),
            Err(err) => {
                if i > 10 {
                    return Err(anyhow!("could not read pin {}: {:?}", pin, err));
                }
                println!("{:?}: sleeping...", err);
                sleep(Duration::from_secs(5));
                i += 1;
            }
        }
    }
}

fn record_sensors(conn: Arc<Mutex<Connection>>) {
    let pins: HashMap<u8, &str> = [(2, "inside")].iter().cloned().collect();
    let wait = Duration::from_secs(5);
    let mut first = true;

    loop {
        for (pin, name) in &pins {
            let reading = match read_sensor(*pin) {
                Ok(r) => r,
                Err(err) => {
                    println!("{}, skipping", err);
                    continue;
                }
            };
            // Ignore first read because it seemed off one time.
            if first {
                first = false;
                continue;
            }
            if let Err(err) = record_reading(&conn, name, reading) {
                println!("could not record in db: {}", err);
            }
        }
        println!("waiting {:?}", wait);
        sleep(wait);
    }
}

fn record_reading(conn: &Arc<Mutex<Connection>>, name: &str, r: Reading) -> Result<()> {
    dbg!("recording {}: {:?}", name, &r);
    let conn = conn.lock().unwrap();
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    conn.execute(
        "INSERT INTO readings VALUES (?, ?, ?), (?, ?, ?)",
        params![
            format!("temp-{}", name),
            now,
            c_to_f(r.temperature),
            format!("humidity-{}", name),
            now,
            r.humidity as f64,
        ],
    )?;
    Ok(())
}

fn c_to_f(c: f32) -> f64 {
    (c * 1.8 + 32.0) as f64
}

fn main() {
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
        record_sensors(record_conn);
    });

    for _ in 0..guards.capacity() {
        let server = server.clone();
        let thread_conn = Arc::clone(&conn);

        let guard = std::thread::spawn(move || loop {
            let req = server.recv().unwrap();
            let url = req.url();
            let url = url.strip_prefix("/").unwrap_or(url);
            let idx = url.find('/');
            let (first, second) = match idx {
                Some(n) => (&url[..n], &url[n + 1..]),
                None => (&url[..], ""),
            };
            let req_conn = Arc::clone(&thread_conn);
            println!("req: {}, {}.", first, second);
            let resp = match first {
                "" => index(),
                "render" => render(req_conn, second),
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

fn render(conn: Arc<Mutex<Connection>>, name: &str) -> Result<Response<Cursor<Vec<u8>>>> {
    let conn = conn.lock().unwrap();
    let mut stmt = conn.prepare("SELECT ts, value FROM readings WHERE name = ?")?;
    let mut rows = stmt.query(params![name])?;

    let mut readings: Vec<(DateTime<Utc>, f64)> = vec![];
    let mut ts_min = Utc::now();
    let mut ts_max = ts_min
        .checked_sub_signed(chrono::Duration::weeks(1))
        .unwrap();
    let mut val_min = 200.0;
    let mut val_max = -200.0;
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
        println!("{}, {}", ts, val);
    }
    if readings.is_empty() || ts_min == ts_max {
        return Err(anyhow!("no data"));
    }
    if val_min == val_max {
        val_min -= 10.0;
        val_max += 10.0;
    }
    println!(
        "{}, {}, {}, {}, {}",
        ts_min,
        ts_max,
        val_min,
        val_max,
        readings.len()
    );

    let mut data = String::with_capacity(1024);
    {
        let root = SVGBackend::with_string(&mut data, (640, 480)).into_drawing_area();
        root.fill(&WHITE)?;
        let mut chart = ChartBuilder::on(&root)
            .caption(name, ("sans-serif", 30).into_font())
            .margin(5)
            .x_label_area_size(30)
            .y_label_area_size(30)
            .build_cartesian_2d(ts_min..ts_max, val_min..val_max)?;

        chart
            .configure_mesh()
            .x_label_formatter(&|d| d.format("%a %R").to_string())
            .draw()?;

        chart.draw_series(LineSeries::new(readings, &RED))?;
    }

    Ok(Response::from_data(data).with_header(
        tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"image/svg+xml"[..]).unwrap(),
    ))
}

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
            r#"INSERT INTO readings VALUES ("temp-outside", ?, ?);"#,
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
				cheese cave control BLAH
			</a>
		</h3>
		<div>
			<img src="/render/temp-inside" alt="inside temp" class="img" />
			<img src="/render/temp-outside" alt="inside temp" class="img" />
		</div>
	</body>
</html>
"#;
