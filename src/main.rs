// Try to cross compile with https://github.com/japaric/rust-cross
// Probably just create a dockerfile that can do it
// cargo build --target=armv7-unknown-linux-gnueabihf

use anyhow::Result;
use chrono::prelude::*;
use plotters::prelude::*;
use rand::prelude::*;
use rusqlite::{params, Connection};
use rust_embed::RustEmbed;
use std::cmp::{max, min};
use std::io::Cursor;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tiny_http::{Header, Response, Server, StatusCode};

#[derive(RustEmbed)]
#[folder = "assets/"]
struct Asset;

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
    Ok(html_response(Asset::get("index.html").unwrap()))
}

fn render(conn: Arc<Mutex<Connection>>, name: &str) -> Result<Response<Cursor<Vec<u8>>>> {
    let conn = conn.lock().unwrap();
    let mut stmt = conn.prepare("SELECT ts, value FROM readings WHERE name = ?")?;
    let mut rows = stmt.query(params![name])?;

    let mut readings: Vec<(DateTime<Utc>, f64)> = vec![];
    let mut ts_min = Utc::now();
    let mut ts_max = Utc.timestamp(0, 0);
    let mut val_min = f64::MAX;
    let mut val_max = f64::MIN;
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

    let mut data = String::with_capacity(1024);
    {
        let root = SVGBackend::with_string(&mut data, (640, 480)).into_drawing_area();
        root.fill(&WHITE)?;
        let mut chart = ChartBuilder::on(&root)
            .caption(name, ("sans-serif", 30).into_font())
            .margin(5)
            .x_label_area_size(30)
            .y_label_area_size(30)
            .build_ranged(ts_min..ts_max, val_min..val_max)?;

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
    sample_data(&conn)?;
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
