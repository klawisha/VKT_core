mod color;
mod middlewares;
mod util;

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use time;
use clap;
use htmlescape::encode_minimal;
use iron::headers;
use iron::headers::{AcceptEncoding, ContentEncoding, Encoding, QualityItem};
use iron::method;
use iron::modifiers::Redirect;
use iron::status;
use iron::{Chain, Handler, Iron, IronError, IronResult, Request, Response, Set};
use iron_cors::CorsMiddleware;
use lazy_static::lazy_static;
use mime_guess as mime_types;
use multipart::server::{Multipart, SaveResult};
use open;
use path_dedot::ParseDot;
use percent_encoding::percent_decode;
use pretty_bytes::converter::convert;
use rand::distributions::Alphanumeric;
use rand::{thread_rng, Rng};
use termcolor::{Color, ColorSpec};

use color::{build_spec, Printer};

use time::Time;

use util::{
    enable_string, encode_link_path, error_io2iron, error_resp, now_string,
    system_time_to_date_time, StringError, ROOT_LINK,
};

use middlewares::{AuthChecker, CompressionHandler, RequestLogger};

const ORDER_ASC: &str = "asc";
const ORDER_DESC: &str = "desc";
const DEFAULT_ORDER: &str = ORDER_DESC;

lazy_static! {
    static ref SORT_FIELDS: Vec<&'static str> = vec!["name", "modified", "size"];
}

fn main() {
    let matches = clap::App::new("VKR server")
        .setting(clap::AppSettings::ColoredHelp)
        .version("1.0.0")
        .arg(clap::Arg::with_name("root")
             .index(1)
             .validator(|s| {
                 match fs::metadata(s) {
                     Ok(metadata) => {
                         if metadata.is_dir() { Ok(()) } else {
                             Err("Not directory".to_owned())
                         }
                     },
                     Err(e) => Err(e.to_string())
                 }
             })
             .help("Root directory"))
        .arg(clap::Arg::with_name("index")
             .short('i')
             .long("index")
             .help("Enable automatic render index page [index.html, index.htm]"))
        .arg(clap::Arg::with_name("upload")
             .short('u')
             .long("upload")
             .help("Enable upload files. (multiple select) (CSRF token required)"))
        .arg(clap::Arg::with_name("redirect").long("redirect")
             .takes_value(true)
             .validator(|url_string| iron::Url::parse(url_string).map(|_| ()))
             .help("takes a URL to redirect to using HTTP 301 Moved Permanently"))
        .arg(clap::Arg::with_name("nosort")
             .long("nosort")
             .help("Disable directory entries sort (by: name, modified, size)"))
        .arg(clap::Arg::with_name("nocache")
             .long("nocache")
             .help("Disable http cache"))
        .arg(clap::Arg::with_name("norange")
             .long("norange")
             .help("Disable header::Range support (partial request)"))
        .arg(clap::Arg::with_name("cert")
             .long("cert")
             .takes_value(true)
             .validator(|s| {
                 match fs::metadata(s) {
                     Ok(metadata) => {
                         if metadata.is_file() { Ok(()) } else {
                             Err("Not a regular file".to_owned())
                         }
                     },
                     Err(e) => Err(e.to_string())
                 }
             })
             .help("TLS/SSL certificate (pkcs#12 format)"))
        .arg(clap::Arg::with_name("cors")
             .long("cors")
             .help("Enable CORS via the \"Access-Control-Allow-Origin\" header"))
        .arg(clap::Arg::with_name("certpass").
             long("certpass")
             .takes_value(true)
             .help("TLS/SSL certificate password"))
        .arg(clap::Arg::with_name("upload_size_limit")
             .short('l')
             .long("upload-size-limit")
             .takes_value(true)
             .default_value("8000000")
             .value_name("NUM")
             .validator(|s| {
                 match s.parse::<u64>() {
                     Ok(_) => Ok(()),
                     Err(e) => Err(e.to_string())
                 }})
             .help("Upload file size limit [bytes]"))
        .arg(clap::Arg::with_name("ip")
             .long("ip")
             .takes_value(true)
             .default_value("127.0.0.1")
             .validator(|s| {
                 match IpAddr::from_str(&s) {
                     Ok(_) => Ok(()),
                     Err(e) => Err(e.to_string())
                 }
             })
             .help("IP address to bind"))
        .arg(clap::Arg::with_name("port")
             .short('p')
             .long("port")
             .takes_value(true)
             .default_value("8000")
             .validator(|s| {
                 match s.parse::<u16>() {
                     Ok(_) => Ok(()),
                     Err(e) => Err(e.to_string())
                 }
             })
             .help("Port number"))
        .arg(clap::Arg::with_name("threads")
             .short('t')
             .long("threads")
             .takes_value(true)
             .default_value("3")
             .validator(|s| {
                 match s.parse::<u8>() {
                     Ok(v) => {
                         if v > 0 { Ok(()) } else {
                             Err("Not positive number".to_owned())
                         }
                     }
                     Err(e) => Err(e.to_string())
                 }
             })
             .help("How many worker threads"))
        .arg(clap::Arg::with_name("try-file-404")
             .long("try-file")
             .visible_alias("try-file-404")
             .takes_value(true)
             .value_name("PATH")
             .validator(|s| {
                 match fs::metadata(s) {
                     Ok(metadata) => {
                         if metadata.is_file() { Ok(()) } else {
                             Err("Not a file".to_owned())
                         }
                     },
                     Err(e) => Err(e.to_string())
                 }
             })
             .help("serve this file (server root relative) in place of missing files (useful for single page apps)"))
        .arg(clap::Arg::with_name("silent")
             .long("silent")
             .short('s')
             .takes_value(false)
             .help("Disable all outputs"))
        .arg(clap::Arg::with_name("open")
             .long("open")
             .short('o')
             .help("Open the page in the default browser"))
        .get_matches();

    let root = matches
        .value_of("root")
        .map(|s| PathBuf::from(s).canonicalize().unwrap())
        .unwrap_or_else(|| env::current_dir().unwrap());
    let index = matches.is_present("index");
    let upload_arg = matches.is_present("upload");
    let redirect_to = matches
        .value_of("redirect")
        .map(iron::Url::parse)
        .map(Result::unwrap);
    let sort = !matches.is_present("nosort");
    let cache = !matches.is_present("nocache");
    let range = !matches.is_present("norange");
    let cert = matches.value_of("cert");
    let certpass = matches.value_of("certpass");
    let cors = matches.is_present("cors");
    let ip = matches.value_of("ip").unwrap();
    let port = matches.value_of("port").unwrap().parse::<u16>().unwrap();
    let upload_size_limit = matches
        .value_of("upload_size_limit")
        .unwrap()
        .parse::<u64>()
        .unwrap();
    let threads = matches.value_of("threads").unwrap().parse::<u8>().unwrap();
    let try_file_404 = matches.value_of("try-file-404");

    let printer = Printer::new();
    let color_blue = Some(build_spec(Some(Color::Blue), false));
    let color_red = Some(build_spec(Some(Color::Red), false));
    let addr = format!("{}:{}", ip, port);
  
    let open = matches.is_present("open");

    if open {
        let host = format!("http://{}", &addr);

        match open::that(&host) {
            Ok(_) => println!("Openning {} in default browser", &host),
            Err(err) => eprintln!("Unable to open in default browser {}", err.to_string()),
        }
    }

    let silent = matches.is_present("silent");

    let upload: Option<Upload> = if upload_arg {
        let token: String = thread_rng()
            .sample_iter(&Alphanumeric)
            .take(10)
            .map(char::from)
            .collect();
        Some(Upload { csrf_token: token })
    } else {
        None
    };

    if !silent {
        printer
            .println_out(
                r#"     Index: {}, Cache: {}, Cors: {}, Range: {}, Sort: {}, Threads: {}
          Upload: {}, CSRF Token: {}
          https: {}, Cert: {}, Cert-Password: {}
          Root: {},
    TryFile404: {}
       Address: {}
    ======== [{}] ========"#,
                &vec![
                    enable_string(index),
                    enable_string(cache),
                    enable_string(cors),
                    enable_string(range),
                    enable_string(sort),
                    threads.to_string(),
                    enable_string(upload_arg),
                    (if upload.is_some() {
                        upload.as_ref().unwrap().csrf_token.as_str()
                    } else {
                        ""
                    })
                    .to_string(),
                    (if cert.is_some() {
                        "enabled"
                    } else {
                        "disabled"
                    })
                    .to_string(),
                    cert.unwrap_or("").to_owned(),
                    certpass.unwrap_or("").to_owned(),
                    root.to_str().unwrap().to_owned(),
                    try_file_404.unwrap_or("").to_owned(),
                    format!(
                        "{}://{}",
                        if cert.is_some() { "https" } else { "http" },
                        addr
                    ),
                    now_string(),
                ]
                .iter()
                .map(|s| (s.as_str(), &color_blue))
                .collect::<Vec<(&str, &Option<ColorSpec>)>>(),
            )
            .unwrap();
    }

    let mut chain = Chain::new(MainHandler {
        root,
        index,
        upload,
        cache,
        range,
        redirect_to,
        sort,
        try_file_404: try_file_404.map(PathBuf::from),
        upload_size_limit,
    });
    if cors {
        chain.link_around(CorsMiddleware::with_allow_any());
    }
 
 
    if !silent {
        chain.link_after(RequestLogger {
            printer: Printer::new(),
        });
    }
    let mut server = Iron::new(chain);
    server.threads = threads as usize;

    #[cfg(feature = "tls")]
    let rv = if let Some(cert) = cert {
        use hyper_native_tls::NativeTlsServer;
        let ssl = NativeTlsServer::new(cert, certpass.unwrap_or("")).unwrap();
        server.https(&addr, ssl)
    } else {
        server.http(&addr)
    };
    #[cfg(not(feature = "tls"))]
    let rv = if cert.is_some() {
        printer
            .println_err(
                "{}: TLS support is not enabled during compilation of simple-http-server",
                &[("ERROR", &Some(build_spec(Some(Color::Red), true)))],
            )
            .unwrap();
        std::process::exit(1)
    } else {
        server.http(&addr)
    };

    if let Err(e) = rv {
        printer
            .println_err(
                "{}: Can not bind on {}, {}",
                &[
                    ("ERROR", &Some(build_spec(Some(Color::Red), true))),
                    (addr.as_str(), &None),
                    (e.to_string().as_str(), &None),
                ],
            )
            .unwrap();
        std::process::exit(1);
    };
}
struct Upload {
    csrf_token: String,
}

struct MainHandler {
    root: PathBuf,
    index: bool,
    upload: Option<Upload>,
    cache: bool,
    range: bool,
    redirect_to: Option<iron::Url>,
    sort: bool,
    try_file_404: Option<PathBuf>,
    upload_size_limit: u64,
}

impl Handler for MainHandler {
    fn handle(&self, req: &mut Request) -> IronResult<Response> {
        let mut fs_path = self.root.clone();
        if let Some(url) = &self.redirect_to {
            return Ok(Response::with((
                status::PermanentRedirect,
                Redirect(url.clone()),
            )));
        }
        let path_prefix = req
            .url
            .path()
            .into_iter()
            .filter(|s| !s.is_empty())
            .map(|s| {
                percent_decode(s.as_bytes())
                    .decode_utf8()
                    .map(|path| PathBuf::from(&*path))
                    .map_err(|_err| {
                        IronError::new(
                            StringError(format!("invalid path: {}", s)),
                            status::BadRequest,
                        )
                    })
            })
            .collect::<Result<Vec<PathBuf>, _>>()?
            .into_iter()
            .collect::<PathBuf>();
        fs_path.push(&path_prefix);
        let fs_path = fs_path.parse_dot().unwrap();

        if !fs_path.starts_with(&self.root) {
            return Err(IronError::new(
                io::Error::new(io::ErrorKind::PermissionDenied, "Permission Denied"),
                status::Forbidden,
            ));
        }

        if self.upload.is_some() && req.method == method::Post {
            if let Err((s, msg)) = self.save_files(req, &fs_path.to_path_buf()) {
                return Ok(error_resp(s, &msg));
            } else {
                return Ok(Response::with((status::Found, Redirect(req.url.clone()))));
            }
        }

        let path_metadata = match fs::metadata(&fs_path) {
            Ok(value) => value,
            Err(err) => {
                let status = match err.kind() {
                    io::ErrorKind::PermissionDenied => status::Forbidden,
                    io::ErrorKind::NotFound => {
                        if let Some(ref p) = self.try_file_404 {
                            if Some(true) == fs::metadata(p).ok().map(|meta| meta.is_file()) {
                                return self.send_file(req, p);
                            }
                        }
                        status::NotFound
                    }
                    _ => status::InternalServerError,
                };
                return Err(IronError::new(err, status));
            }
        };

        if path_metadata.is_dir() {
            let path_prefix: Vec<String> = path_prefix
                .iter()
                .map(|s| s.to_string_lossy().to_string())
                .collect();
            self.list_directory(req, &fs_path.to_path_buf(), &path_prefix)
        } else {
            self.send_file(req, &fs_path)
        }
    }
}

impl MainHandler {
    fn save_files(
        &self,
        req: &mut Request,
        path: &PathBuf,
    ) -> Result<(), (status::Status, String)> {
        match Multipart::from_request(req) {
            Ok(mut multipart) => {
                // Получение всех данных и их обработка.
                // save().temp() полностью читает запрос, анализирует все поля и сохраняет все файлы
                // в новом временном каталоге во временном каталоге ОС.

                // Fetching all data and processing it.
                // save().temp() reads the request fully, parsing all fields and saving all files
                // in a new temporary directory under the OS temporary directory.
                match multipart.save().size_limit(self.upload_size_limit).temp() {
                    SaveResult::Full(entries) => {
                        // Извлечь поле csrf, чтобы проверить, соответствует ли токен сгенерированному

                        // Pull out csrf field to check if token matches one generated
                        let csrf_field = match entries
                            .fields
                            .get("csrf")
                            .map(|fields| fields.first())
                            .unwrap_or(None)
                        {
                            Some(field) => field,
                            None => {
                                return Err((
                                    status::BadRequest,
                                    String::from("csrf parameter not provided"),
                                ))
                            }
                        };

                        // Читаем значение токена из поля
                        // Read token value from field
                        let mut token = String::new();
                        csrf_field
                            .data
                            .readable()
                            .unwrap()
                            .read_to_string(&mut token)
                            .unwrap();

                        // Если совпадает   
                        // Check if they match
                        if self.upload.as_ref().unwrap().csrf_token != token {
                            return Err((
                                status::BadRequest,
                                String::from("csrf token does not match"),
                            ));
                        }

                        // Берём все поля именованые files
                        // Grab all the fields named files
                        let files_fields = match entries.fields.get("files") {
                            Some(fields) => fields,
                            None => {
                                return Err((status::BadRequest, String::from("no files provided")))
                            }
                        };

                        for field in files_fields {
                            let mut data = field.data.readable().unwrap();
                            let headers = &field.headers;
                            let mut target_path = path.clone();

                            target_path.push(headers.filename.clone().unwrap());
                            if let Err(errno) = std::fs::File::create(target_path)
                                .and_then(|mut file| io::copy(&mut data, &mut file))
                            {
                                return Err((
                                    status::InternalServerError,
                                    format!("Copy file failed: {}", errno),
                                ));
                            } else {
                                println!("  >> File saved: {}", headers.filename.clone().unwrap());
                            }
                        }
                        Ok(())
                    }
                    SaveResult::Partial(_entries, reason) => {
                        Err((status::InternalServerError, reason.unwrap_err().to_string()))
                    }
                    SaveResult::Error(error) => {
                        Err((status::InternalServerError, error.to_string()))
                    }
                }
            }
            Err(_) => Err((
                status::BadRequest,
                "The request is not multipart".to_owned(),
            )),
        }
    }

    fn list_directory(
        &self,
        req: &mut Request,
        fs_path: &PathBuf,
        path_prefix: &[String],
    ) -> IronResult<Response> {
        struct Entry {
            filename: String,
            metadata: fs::Metadata,
        }

        let mut resp = Response::with(status::Ok);
        let mut fs_path = fs_path.clone();
        let mut rows = Vec::new();

        let read_dir = fs::read_dir(&fs_path).map_err(error_io2iron)?;
        let mut entries = Vec::new();
        for entry_result in read_dir {
            let entry = entry_result.map_err(error_io2iron)?;
            entries.push(Entry {
                filename: entry.file_name().into_string().unwrap(),
                metadata: entry.metadata().map_err(error_io2iron)?,
            });
        }
        
        // навигационная цепочка
        // Breadcrumb navigation
        let breadcrumb = if !path_prefix.is_empty() {
            let mut breadcrumb = path_prefix.to_owned();
            let mut bread_links: Vec<String> = Vec::new();
            bread_links.push(breadcrumb.pop().unwrap());
            while !breadcrumb.is_empty() {
                bread_links.push(format!(
                    r#"<a href="/{link}/"><strong>{label}</strong></a>"#,
                    link = encode_link_path(&breadcrumb),
                    label = encode_minimal(&breadcrumb.pop().unwrap().to_owned()),
                ));
            }
            bread_links.push(ROOT_LINK.to_owned());
            bread_links.reverse();
            bread_links.join(" / ")
        } else {
            ROOT_LINK.to_owned()
        };

        // Сортировка ссылок
        // Sort links
        let sort_links = if self.sort {
            let mut sort_field = Some(String::from("name"));
            let mut order = None;
            for (k, v) in req.url.as_ref().query_pairs() {
                if k == "sort" {
                    sort_field = Some(v.to_string());
                } else if k == "order" {
                    order = Some(v.to_string());
                }
            }
            let order = order.unwrap_or_else(|| DEFAULT_ORDER.to_owned());
            let mut order_labels = BTreeMap::new();
            for field in SORT_FIELDS.iter() {
                if sort_field == Some((*field).to_owned()) && order == ORDER_DESC {
                    // обратный порядок поля
                    // reverse the order of the field
                    order_labels.insert(field.to_owned(), ORDER_ASC);
                }
            }

            if let Some(field) = sort_field {
                if SORT_FIELDS
                    .iter()
                    .position(|s| *s == field.as_str())
                    .is_none()
                {
                    return Err(IronError::new(
                        StringError(format!("Unknown sort field: {}", field)),
                        status::BadRequest,
                    ));
                }
                if vec![ORDER_ASC, ORDER_DESC]
                    .iter()
                    .position(|s| *s == order)
                    .is_none()
                {
                    return Err(IronError::new(
                        StringError(format!("Unknown sort order: {}", order)),
                        status::BadRequest,
                    ));
                }

                let reverse = order == ORDER_DESC;
                entries.sort_by(|a, b| {
                    let rv = match field.as_str() {
                        "name" => a.filename.cmp(&b.filename),
                        "modified" => {
                            let a = a.metadata.modified().unwrap();
                            let b = b.metadata.modified().unwrap();
                            a.cmp(&b)
                        }
                        "size" => {
                            if a.metadata.is_dir() == b.metadata.is_dir()
                                || a.metadata.is_file() == b.metadata.is_file()
                            {
                                a.metadata.len().cmp(&b.metadata.len())
                            } else if a.metadata.is_dir() {
                                Ordering::Less
                            } else {
                                Ordering::Greater
                            }
                        }
                        _ => unreachable!(),
                    };
                    if reverse {
                        rv.reverse()
                    } else {
                        rv
                    }
                });
            }

            let mut current_link = path_prefix.to_owned();
            current_link.push("".to_owned());
            // Формируем таблицу
            // Creating a table
            format!(
                r#"
<tr>
  <th><a href="/{link}?sort=name&order={name_order}">Name</a></th>
  <th><a href="/{link}?sort=modified&order={modified_order}">Last modified</a></th>
  <th><a href="/{link}?sort=size&order={size_order}">Size</a></th>
</tr>
<tr><td style="border-top:1px dashed #BBB;" colspan="5"></td></tr>
"#,
                link = encode_link_path(&current_link),
                name_order = order_labels.get("name").unwrap_or(&DEFAULT_ORDER),
                modified_order = order_labels.get("modified").unwrap_or(&DEFAULT_ORDER),
                size_order = order_labels.get("size").unwrap_or(&DEFAULT_ORDER)
            )
        } else {
            "".to_owned()
        };

        // Перейти к ссылке на родительский каталог
        // Goto parent directory link
        if !path_prefix.is_empty() {
            let mut link = path_prefix.to_owned();
            link.pop();
            if !link.is_empty() {
                link.push("".to_owned());
            }
            rows.push(format!(
                r#"
<tr>
  <td><a href="/{link}"><strong>[Up]</strong></a></td>
  <td></td>
  <td></td>
</tr>
"#,
                link = encode_link_path(&link)
            ));
        } else {
            rows.push(r#"<tr><td>&nbsp;</td></tr>"#.to_owned());
        }

        // Записи каталога
        // Directory entries
        for Entry { filename, metadata } in entries {
            if self.index {
                for fname in &["index.html", "index.htm"] {
                    if filename == *fname {

                        // Автом рендер index.html
                        // Automatic render index page
                        fs_path.push(filename);
                        return self.send_file(req, &fs_path);
                    }
                }
            }
            // Время - дата модификации
            // * Entry.modified
            let file_modified = system_time_to_date_time(metadata.modified().unwrap())
                .format("%Y-%m-%d %H:%M:%S")
                .to_string();
            // Размер файла
            // * Entry.filesize
            let file_size = if metadata.is_dir() {
                "-".to_owned()
            } else {
                convert(metadata.len() as f64)
            };

            // Стиль ссылки
            // * Entry.linkstyle
            let link_style = if metadata.is_dir() {
                "style=\"font-weight: bold;\"".to_owned()
            } else {
                "".to_owned()
            };
            // Ссылка
            // * Entry.link
            let mut link = path_prefix.to_owned();
            link.push(filename.clone());
            if metadata.is_dir() {
                link.push("".to_owned());
            }
            //Лейбел
            // * Entry.label
            let file_name_label = if metadata.is_dir() {
                format!("{}/", &filename)
            } else {
                filename.clone()
            };
            // Рендер одной записи в директории
            // Render one directory entry
            rows.push(format!(
                r#"
<tr>
  <td><a {linkstyle} href="/{link}">{label}</a></td>
  <td style="color:#888;">[{modified}]</td>
  <td style="color:#00CC33;"><bold>{filesize}</bold></td>
</tr>
"#,
                linkstyle = link_style,
                link = encode_link_path(&link),
                label = encode_minimal(&file_name_label),
                modified = file_modified,
                filesize = file_size
            ));
        }

        // Необязательная форма загрузки
        // Optional upload form
        let upload_form = if self.upload.is_some() {
            format!(
                r#"
<form style="margin-top:1em; margin-bottom:1em;" action="/{path}" method="POST" enctype="multipart/form-data">
  <input type="file" name="files" accept="*" multiple />
  <input type="hidden" name="csrf" value="{csrf}"/>
  <input type="submit" value="Upload" />
</form>
"#,
                path = encode_link_path(path_prefix),
                csrf = self.upload.as_ref().unwrap().csrf_token
            )
        } else {
            "".to_owned()
        };

        // Собираем все части вместе
        // Put all parts together
        resp.set_mut(format!(
            r#"<!DOCTYPE html>
<html>
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width,initial-scale=1.0, minimum-scale=1.0, maximum-scale=1.0, user-scalable=no"/>
  <style> 
  a {{ text-decoration:none; }} 
  html {{ min-height: 100%; }}
  body {{ -webkit-animation: colorchange 60s infinite; animation: colorchange 60s infinite; background: linear-gradient(#336666, #339933); background-color: #336666;  background-image: url(back.jpg); background-repeat: no-repeat; background-position: center center; background-attachment: fixed; -webkit-background-size: cover; -moz-background-size: cover; -o-background-size: cover; background-size: cover; }}
  </style>
</head>
<body>
<img src="logo.jpg" align="bottom">
<br>
<h1 style="color:#00CC33">Transporter by V.K.</h1>
  {upload_form}
  <div>{breadcrumb}</div>
  <hr />
  <table>
    {sort_links}
    {rows}
  </table>
</body>
</html>
"#,
            upload_form = upload_form,
            breadcrumb = breadcrumb,
            sort_links = sort_links,
            rows = rows.join("\n")
        ));

        resp.headers.set(headers::ContentType::html());
        Ok(resp)
    }

    fn send_file<P: AsRef<Path>>(&self, req: &Request, path: P) -> IronResult<Response> {
        use filetime::FileTime;
        use iron::headers::{
            AcceptRanges, ByteRangeSpec, ContentLength, ContentRange, ContentRangeSpec,
            ContentType, ETag, EntityTag, IfMatch, IfRange, Range, RangeUnit,
        };
        use iron::headers::{
            CacheControl, CacheDirective, HttpDate, IfModifiedSince, LastModified,
        };
        use iron::method::Method;
        use iron::mime::{Mime, SubLevel, TopLevel};

        let path = path.as_ref();
        let metadata = fs::metadata(path).map_err(error_io2iron)?;

        let time = FileTime::from_last_modification_time(&metadata);
        
        let mut resp = Response::with(status::Ok);
        if self.range {
            resp.headers.set(AcceptRanges(vec![RangeUnit::Bytes]));
        }
        match req.method {
            Method::Head => {
                let content_type = req
                    .headers
                    .get::<ContentType>()
                    .cloned()
                    .unwrap_or_else(|| ContentType(Mime(TopLevel::Text, SubLevel::Plain, vec![])));
                resp.headers.set(content_type);
                resp.headers.set(ContentLength(metadata.len()));
            }
            Method::Get => {
                // Установить MIME-тип
                // Set mime type
                let mime = mime_types::from_path(path).first_or_octet_stream();
                resp.headers
                    .set_raw("content-type", vec![mime.to_string().into_bytes()]);

                if self.range {
                    let mut range = req.headers.get::<Range>();

  
                    match range {
                        Some(&Range::Bytes(ref ranges)) => {
                            if let Some(range) = ranges.get(0) {
                                let (offset, length) = match *range {
                                    ByteRangeSpec::FromTo(x, mut y) => {
                                        // "x-y"
                                        if x >= metadata.len() || x > y {
                                            return Err(IronError::new(
                                                StringError(format!(
                                                    "Invalid range(x={}, y={})",
                                                    x, y
                                                )),
                                                status::RangeNotSatisfiable,
                                            ));
                                        }
                                        if y >= metadata.len() {
                                            y = metadata.len() - 1;
                                        }
                                        (x, y - x + 1)
                                    }
                                    ByteRangeSpec::AllFrom(x) => {
                                        // "x-"
                                        if x >= metadata.len() {
                                            return Err(IronError::new(
                                                StringError(format!(
                                                    "Range::AllFrom to large (x={}), Content-Length: {})",
                                                    x, metadata.len())),
                                                status::RangeNotSatisfiable
                                            ));
                                        }
                                        (x, metadata.len() - x)
                                    }
                                    ByteRangeSpec::Last(mut x) => {
                                        // "-x"
                                        if x > metadata.len() {
                                            x = metadata.len();
                                        }
                                        (metadata.len() - x, x)
                                    }
                                };
                                let mut file = fs::File::open(path).map_err(error_io2iron)?;
                                file.seek(SeekFrom::Start(offset)).map_err(error_io2iron)?;
                                let take = file.take(length);

                                resp.headers.set(ContentLength(length));
                                resp.headers.set(ContentRange(ContentRangeSpec::Bytes {
                                    range: Some((offset, offset + length - 1)),
                                    instance_length: Some(metadata.len()),
                                }));
                                resp.body = Some(Box::new(Box::new(take) as Box<dyn Read + Send>));
                                resp.set_mut(status::PartialContent);
                            } else {
                                return Err(IronError::new(
                                    StringError("Empty range set".to_owned()),
                                    status::RangeNotSatisfiable,
                                ));
                            }
                        }
                        Some(_) => {
                            return Err(IronError::new(
                                StringError("Invalid range type".to_owned()),
                                status::RangeNotSatisfiable,
                            ));
                        }
                        _ => {
                            resp.headers.set(ContentLength(metadata.len()));
                            let file = fs::File::open(path).map_err(error_io2iron)?;
                            resp.body = Some(Box::new(file));
                        }
                    }
                } else {
                    resp.headers.set(ContentLength(metadata.len()));
                    let file = fs::File::open(path).map_err(error_io2iron)?;
                    resp.body = Some(Box::new(file));
                }
            }
            _ => {
                return Ok(Response::with(status::MethodNotAllowed));
            }
        }


        if self.cache {
            static SECONDS: u32 = 7 * 24 * 3600; // max-age: 7 days  //макс. время жизни: 7 дней
            if let Some(&IfModifiedSince(HttpDate(ref if_modified_since))) =
                req.headers.get::<IfModifiedSince>()
            {
              
            };
            let cache = vec![CacheDirective::Public, CacheDirective::MaxAge(SECONDS)];
            resp.headers.set(CacheControl(cache));
        }
        Ok(resp)
    }
}
