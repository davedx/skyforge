use eframe::egui::{self, RichText, TextEdit};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::Duration,
};

#[derive(Default)]
struct Results {
    variables: Vec<String>,
    rows: Vec<RequestResult>,
    worker: Option<Worker>,
    error: Option<String>,
    sort: usize,
    descending: bool,
    filter_open: bool,
    positive: String,
    negative: String,
    draft_positive: String,
    draft_negative: String,
    selected_response: Option<u128>,
}

struct Worker {
    receiver: mpsc::Receiver<RequestResult>,
    stop: Arc<AtomicBool>,
    handle: thread::JoinHandle<()>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

#[derive(Debug, Default)]
struct RequestResult {
    id: u128,
    values: Vec<String>,
    status: Option<u16>,
    seconds: f64,
    error: String,
    timeout: bool,
    length: u64,
    response: String,
    searchable: String,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SecOps {
    pub url: String,
    pub request: String,
    selected: Option<String>,
    values: BTreeMap<String, String>,
    #[serde(skip)]
    results: Results,
}

impl SecOps {
    fn variables(&self) -> Vec<String> {
        let mut variables = Vec::new();
        // Dollar-delimited names are recognized in first-appearance order.
        let mut parts = self.request.split('$').skip(1);
        while let (Some(name), Some(_)) = (parts.next(), parts.next()) {
            if !name.is_empty()
                && !name.chars().any(char::is_whitespace)
                && !variables.iter().any(|v| v == name)
            {
                variables.push(name.to_owned());
            }
        }
        variables
    }

    fn payload_count(&self, variable: &str) -> usize {
        self.values.get(variable).map_or(0, |v| v.lines().count())
    }

    fn request_count(&self, variables: &[String]) -> Option<u128> {
        if variables.is_empty() || variables.iter().any(|v| self.payload_count(v) == 0) {
            return Some(0);
        }
        variables.iter().try_fold(1u128, |count, variable| {
            count.checked_mul(self.payload_count(variable) as u128)
        })
    }

    pub fn running(&self) -> bool {
        self.results.worker.is_some()
    }

    pub fn stop(&self) {
        if let Some(worker) = &self.results.worker {
            worker.stop.store(true, Ordering::SeqCst);
        }
    }

    pub fn shutdown(&mut self) {
        self.stop();
        while self.running() {
            self.poll();
            if self.running() {
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    pub fn poll(&mut self) {
        if let Some(worker) = &self.results.worker {
            self.results.rows.extend(worker.receiver.try_iter());
            if worker.handle.is_finished() {
                let worker = self.results.worker.take().unwrap();
                self.results.rows.extend(worker.receiver.try_iter());
            }
        }
    }

    fn start(&mut self, ctx: &egui::Context) -> Result<(), String> {
        let url = url::Url::parse(self.url.trim()).map_err(|e| format!("Invalid URL: {e}"))?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err("Enter an HTTP or HTTPS endpoint URL.".into());
        }
        let variables = self.variables();
        let total = self
            .request_count(&variables)
            .ok_or("Too many request combinations.")?;
        if total == 0 {
            return Err("Define variables and add at least one value to each list.".into());
        }
        let lists: Vec<Vec<String>> = variables
            .iter()
            .map(|v| self.values[v].lines().map(str::to_owned).collect())
            .collect();
        // Validate before replacing any previous results or sending requests.
        parse_request(&self.request)?;
        let template = self.request.clone();
        let (sender, receiver) = mpsc::sync_channel(64);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker_variables = variables.clone();
        let ctx = ctx.clone();
        let handle = thread::spawn(move || {
            let mut positions = vec![0; lists.len()];
            for id in 1..=total {
                if worker_stop.load(Ordering::SeqCst) {
                    break;
                }
                let values: Vec<String> = lists
                    .iter()
                    .zip(&positions)
                    .map(|(v, i)| v[*i].clone())
                    .collect();
                let request = interpolate(&template, &worker_variables, &values);
                let result = send_request(
                    url.as_str(),
                    &request,
                    id,
                    values,
                    &worker_stop,
                    Duration::from_secs(30),
                );
                if worker_stop.load(Ordering::SeqCst) {
                    break;
                }
                if sender.send(result).is_err() {
                    break;
                }
                ctx.request_repaint();
                if id < total {
                    for _ in 0..10 {
                        if worker_stop.load(Ordering::SeqCst) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                }
                for i in (0..positions.len()).rev() {
                    positions[i] += 1;
                    if positions[i] < lists[i].len() {
                        break;
                    }
                    positions[i] = 0;
                }
            }
            ctx.request_repaint();
        });
        self.results.rows.clear();
        self.results.selected_response = None;
        self.results.variables = variables;
        self.results.sort = 0;
        self.results.descending = false;
        self.results.worker = Some(Worker {
            receiver,
            stop,
            handle,
        });
        Ok(())
    }

    fn results_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Results");
            if ui.button("Filter…").clicked() {
                self.results.draft_positive = self.results.positive.clone();
                self.results.draft_negative = self.results.negative.clone();
                self.results.filter_open = true;
            }
            let filtered = !self.results.positive.is_empty() || !self.results.negative.is_empty();
            if ui
                .add_enabled(filtered, egui::Button::new("Clear filter"))
                .clicked()
            {
                self.results.positive.clear();
                self.results.negative.clear();
                self.results.draft_positive.clear();
                self.results.draft_negative.clear();
            }
            if let Some(worker) = &self.results.worker {
                ui.spinner();
                if ui.button("Stop").clicked() {
                    worker.stop.store(true, Ordering::SeqCst);
                }
            }
        });
        if let Some(error) = &self.results.error {
            ui.colored_label(crate::theme::LINK, error);
        }
        let mut indices: Vec<usize> = self
            .results
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| matches_filters(row, &self.results.positive, &self.results.negative))
            .map(|(i, _)| i)
            .collect();
        let n = self.results.variables.len();
        indices.sort_by(|a, b| {
            let order = compare_rows(
                &self.results.rows[*a],
                &self.results.rows[*b],
                self.results.sort,
                n,
            );
            let order = if self.results.descending {
                order.reverse()
            } else {
                order
            };
            order.then_with(|| self.results.rows[*a].id.cmp(&self.results.rows[*b].id))
        });
        ui.label(format!(
            "{} shown · {} completed",
            indices.len(),
            self.results.rows.len()
        ));
        let mut headers = vec!["Request ID".to_owned()];
        headers.extend(self.results.variables.clone());
        headers.extend(
            [
                "Status code",
                "Response time (ms)",
                "Error",
                "Timeout",
                "Response length",
                "Response",
            ]
            .map(str::to_owned),
        );
        egui::ScrollArea::horizontal()
            .id_salt("secops_table_horizontal")
            .show(ui, |ui| {
                let mut table = egui_extras::TableBuilder::new(ui)
                    .id_salt("secops_results_table")
                    .striped(true)
                    .resizable(true)
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center));
                for column in 0..headers.len() - 1 {
                    let width = if column == 0 {
                        90.0
                    } else if column <= n {
                        130.0
                    } else {
                        match column - n {
                            1 => 100.0,
                            2 | 3 => 150.0,
                            4 => 90.0,
                            _ => 130.0,
                        }
                    };
                    table = table.column(egui_extras::Column::initial(width).at_least(70.0));
                }
                table = table.column(egui_extras::Column::remainder().at_least(180.0));
                table
                    .header(28.0, |mut header| {
                        for (column, label) in headers.iter().enumerate() {
                            header.col(|ui| {
                                let suffix = if self.results.sort == column {
                                    if self.results.descending {
                                        " ↓"
                                    } else {
                                        " ↑"
                                    }
                                } else {
                                    ""
                                };
                                if ui.button(format!("{label}{suffix}")).clicked() {
                                    if self.results.sort == column {
                                        self.results.descending = !self.results.descending;
                                    } else {
                                        self.results.sort = column;
                                        self.results.descending = false;
                                    }
                                }
                            });
                        }
                    })
                    .body(|body| {
                        body.rows(26.0, indices.len(), |mut table_row| {
                            let row = &self.results.rows[indices[table_row.index()]];
                            let mut cells = vec![row.id.to_string()];
                            cells.extend(row.values.clone());
                            cells.extend([
                                row.status.map_or_else(|| "—".into(), |s| s.to_string()),
                                format!("{:.1}", row.seconds * 1000.0),
                                row.error.clone(),
                                if row.timeout { "Yes" } else { "No" }.into(),
                                row.length.to_string(),
                            ]);
                            for cell in cells {
                                table_row.col(|ui| {
                                    ui.add(egui::Label::new(&cell).truncate())
                                        .on_hover_text(&cell);
                                });
                            }
                            table_row.col(|ui| {
                                let preview = response_preview(&row.response);
                                if ui
                                    .add(
                                        egui::Label::new(preview)
                                            .truncate()
                                            .sense(egui::Sense::click()),
                                    )
                                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                                    .on_hover_text("Open complete response headers and body")
                                    .clicked()
                                {
                                    self.results.selected_response = Some(row.id);
                                }
                            });
                        });
                    });
            });
    }

    fn filters_ui(&mut self, ctx: &egui::Context) {
        if !self.results.filter_open {
            return;
        }
        let response = egui::Modal::new(egui::Id::new("secops_filters")).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.heading("Filter requests");
            ui.label("Case-insensitive text, one term per line. Searches requests, responses and result fields.");
            ui.label("Include: match every term");
            ui.add(TextEdit::multiline(&mut self.results.draft_positive).desired_rows(3).desired_width(f32::INFINITY));
            ui.label("Exclude: hide if any term matches");
            ui.add(TextEdit::multiline(&mut self.results.draft_negative).desired_rows(3).desired_width(f32::INFINITY));
            ui.horizontal(|ui| {
                if ui.button("Apply").clicked() {
                    self.results.positive = self.results.draft_positive.clone();
                    self.results.negative = self.results.draft_negative.clone();
                    self.results.filter_open = false;
                }
                if ui.button("Cancel").clicked() { self.results.filter_open = false; }
            });
        });
        if response.should_close() {
            self.results.filter_open = false;
        }
    }

    fn response_ui(&mut self, ctx: &egui::Context) {
        let Some(id) = self.results.selected_response else {
            return;
        };
        let Some(row) = self.results.rows.iter().find(|row| row.id == id) else {
            self.results.selected_response = None;
            return;
        };
        let width = (ctx.screen_rect().width() * 0.9).min(1200.0);
        let height = ctx.screen_rect().height() * 0.8;
        let modal = egui::Modal::new(egui::Id::new("secops_response")).show(ctx, |ui| {
            ui.set_width(width);
            ui.horizontal(|ui| {
                ui.heading(format!("Response · Request {id}"));
                if ui.button("Close").clicked() {
                    self.results.selected_response = None;
                }
            });
            egui::ScrollArea::both()
                .id_salt(("secops_response_scroll", id))
                .auto_shrink([false, false])
                .max_height((height - 50.0).max(100.0))
                .show(ui, |ui| {
                    if row.response.is_empty() {
                        ui.label("No response received.");
                    } else {
                        ui.add(
                            egui::Label::new(RichText::new(&row.response).monospace())
                                .selectable(true)
                                .wrap_mode(egui::TextWrapMode::Extend),
                        );
                    }
                });
        });
        if modal.should_close() {
            self.results.selected_response = None;
        }
    }

    /// Returns whether any editable state changed, so the host can save the draft.
    pub fn show(&mut self, ctx: &egui::Context) -> bool {
        self.poll();
        if self.running() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
        let mut changed = false;
        egui::TopBottomPanel::bottom("secops_results_panel")
            .resizable(true)
            .default_height(220.0)
            .min_height(120.0)
            .max_height((ctx.available_rect().height() - 230.0).max(120.0))
            .show(ctx, |ui| {
                ui.set_min_height(ui.available_height());
                self.results_ui(ui);
            });
        egui::CentralPanel::default().show(ctx, |ui| {
            let spacing = ui.spacing().item_spacing.x;
            let width = (ui.available_width() - spacing) / 3.0;
            let height = ui.available_height();
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(width * 2.0, height),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.heading("SecOps");
                        ui.label("URL");
                        ui.horizontal(|ui| {
                            let button_width = 164.0;
                            changed |= ui.add_sized([ (ui.available_width() - button_width - ui.spacing().item_spacing.x).max(40.0), ui.spacing().interact_size.y ],
                                TextEdit::singleline(&mut self.url).hint_text("https://example.com/login")
                            ).changed();
                            if ui.add_enabled(!self.running(), egui::Button::new("Enumerate requests")).clicked() {
                                self.results.error = self.start(ctx).err();
                            }
                        });
                        ui.add_space(8.0);
                        ui.label(RichText::new("Request payload").strong());
                        ui.label("Mark variables with $name$, for example $p3val$.");
                        egui::ScrollArea::both().id_salt("secops_request_scroll")
                            .auto_shrink([false, false]).show(ui, |ui| {
                                changed |= ui.add_sized(ui.available_size(),
                                    TextEdit::multiline(&mut self.request)
                                        .id_salt("secops_request")
                                        .font(egui::TextStyle::Monospace)
                                        .hint_text("POST /login HTTP/1.0\nCookie: c=cval\nContent-Length: 17\n\nusername=$p3val$&password=$p4val$")
                                        .desired_width(f32::INFINITY)
                                ).changed();
                            });
                    },
                );
                let variables = self.variables();
                if !self.selected.as_ref().is_some_and(|v| variables.contains(v)) {
                    let next = variables.first().cloned();
                    changed |= self.selected != next;
                    self.selected = next;
                }
                ui.allocate_ui_with_layout(
                    egui::vec2(width, height),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.heading("Payload parameters");
                        ui.label("Payload variable");
                        let before = self.selected.clone();
                        egui::ComboBox::from_id_salt("secops_variable")
                            .selected_text(self.selected.as_ref().map_or_else(
                                || "No variables defined".to_owned(), |v| format!("${v}$")))
                            .width(ui.available_width() - 8.0)
                            .show_ui(ui, |ui| {
                                for variable in &variables {
                                    ui.selectable_value(&mut self.selected, Some(variable.clone()), format!("${variable}$"));
                                }
                            });
                        changed |= before != self.selected;
                        ui.add_space(8.0);
                        // Reserve count labels above the editor, then fill them after edits.
                        let (count_rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 48.0), egui::Sense::hover());
                        ui.label(RichText::new("Variable values").strong());
                        ui.label("One value per line");
                        if let Some(selected) = &self.selected {
                            egui::ScrollArea::both().id_salt(("secops_values_scroll", selected))
                                .auto_shrink([false, false]).show(ui, |ui| {
                                    changed |= ui.add_sized(ui.available_size(),
                                        TextEdit::multiline(self.values.entry(selected.clone()).or_default())
                                            .id_salt(("secops_values", selected))
                                            .font(egui::TextStyle::Monospace)
                                            .desired_width(f32::INFINITY)
                                    ).changed();
                                });
                        } else {
                            ui.label("Add a $variable$ to the request to edit its values.");
                        }
                        ui.scope_builder(egui::UiBuilder::new().max_rect(count_rect), |ui| {
                            let count = self.selected.as_ref().map_or(0, |v| self.payload_count(v));
                            ui.label(format!("Payload count: {count}"));
                            let requests = self.request_count(&variables).map_or_else(
                                || "Exceeds numeric limit".to_owned(), |n| n.to_string());
                            ui.label(format!("Request count: {requests}"))
                                .on_hover_text("All combinations: multiply the payload counts of the defined variables.");
                        });
                    },
                );
            });
        });
        self.filters_ui(ctx);
        self.response_ui(ctx);
        changed
    }
}

fn interpolate(template: &str, variables: &[String], values: &[String]) -> String {
    // Single pass prevents dollar signs in payload values from being replaced again.
    let parts: Vec<&str> = template.split('$').collect();
    let mut output = parts[0].to_owned();
    let mut i = 1;
    while i < parts.len() {
        if i + 1 < parts.len() {
            if let Some(index) = variables.iter().position(|v| v == parts[i]) {
                output.push_str(&values[index]);
            } else {
                output.push('$');
                output.push_str(parts[i]);
                output.push('$');
            }
            output.push_str(parts[i + 1]);
            i += 2;
        } else {
            output.push('$');
            output.push_str(parts[i]);
            i += 1;
        }
    }
    output
}

struct HttpRequest {
    method: String,
    target: Option<String>,
    headers: Vec<String>,
    body: String,
    http10: bool,
}

fn parse_request(request: &str) -> Result<HttpRequest, String> {
    let first = request.lines().next().unwrap_or_default();
    let fields: Vec<&str> = first.split_whitespace().collect();
    if fields.len() == 3 && fields[2].starts_with("HTTP/") {
        if !fields[0].bytes().all(|b| b.is_ascii_alphabetic()) {
            return Err("Invalid HTTP method.".into());
        }
        let (head, body) = request
            .split_once("\r\n\r\n")
            .or_else(|| request.split_once("\n\n"))
            .unwrap_or((request, ""));
        let mut headers = Vec::new();
        for header in head.lines().skip(1) {
            let (name, _) = header
                .split_once(':')
                .ok_or("Invalid request header: expected Name: value.")?;
            // The HTTP client recalculates framing after interpolation.
            if !name.eq_ignore_ascii_case("content-length")
                && !name.eq_ignore_ascii_case("transfer-encoding")
            {
                headers.push(header.to_owned());
            }
        }
        Ok(HttpRequest {
            method: fields[0].into(),
            target: Some(fields[1].into()),
            headers,
            body: body.into(),
            http10: fields[2] == "HTTP/1.0",
        })
    } else {
        Ok(HttpRequest {
            method: "POST".into(),
            target: None,
            headers: vec![],
            body: request.into(),
            http10: false,
        })
    }
}

fn destination_url(base: &str, target: Option<&str>) -> Result<String, String> {
    let base = url::Url::parse(base).map_err(|e| format!("Invalid URL: {e}"))?;
    let destination = match target {
        // Preserve the raw path and query, including encoded characters and dot
        // segments, for security training requests. The URL field supplies the origin.
        Some(target) if target.starts_with('/') => {
            format!("{}{target}", &base[..url::Position::BeforePath])
        }
        Some(target) => base
            .join(target)
            .map_err(|e| format!("Invalid request path: {e}"))?
            .to_string(),
        None => base.to_string(),
    };
    Ok(destination)
}

fn send_request(
    url: &str,
    request: &str,
    id: u128,
    values: Vec<String>,
    stop: &AtomicBool,
    timeout: Duration,
) -> RequestResult {
    let mut row = RequestResult {
        id,
        values,
        ..Default::default()
    };
    let mut destination = url.to_owned();
    let outcome = (|| -> Result<(), String> {
        let parsed = parse_request(request)?;
        destination = destination_url(url, parsed.target.as_deref())?;
        let mut command = Command::new("/usr/bin/curl");
        command.args([
            "-q",
            "--silent",
            "--show-error",
            "--globoff",
            "--path-as-is",
            "--include",
            "--proto",
            "=http,https",
            "--max-time",
            &timeout.as_secs_f64().to_string(),
            "--request",
            &parsed.method,
        ]);
        if parsed.http10 {
            command.arg("--http1.0");
        } else {
            command.arg("--http1.1");
        }
        if !parsed
            .headers
            .iter()
            .any(|h| h.to_ascii_lowercase().starts_with("content-type:"))
        {
            command.args(["--header", "Content-Type:"]);
        }
        for header in parsed.headers {
            command.arg("--header").arg(header);
        }
        let has_body = !parsed.body.is_empty();
        if has_body {
            command.args(["--data-binary", "@-"]);
        }
        command.args([
            "--write-out",
            "\nSKYFORGE_META:%{http_code}\t%{time_total}\t%{size_download}\t%{exitcode}",
            "--url",
            &destination,
        ]);
        let (success, bytes, errors) = crate::local::execute(
            command,
            has_body.then_some(parsed.body),
            stop,
            timeout + Duration::from_secs(2),
            usize::MAX,
        )?;
        let output = String::from_utf8_lossy(&bytes);
        let (response, metadata) = output
            .rsplit_once("\nSKYFORGE_META:")
            .ok_or("Missing HTTP response metadata.")?;
        let fields: Vec<&str> = metadata.trim().split('\t').collect();
        row.status = fields
            .first()
            .and_then(|s| s.parse().ok())
            .filter(|s| *s != 0);
        row.seconds = fields
            .get(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or_default();
        row.length = fields
            .get(2)
            .and_then(|s| s.parse().ok())
            .unwrap_or_default();
        row.timeout = fields.get(3) == Some(&"28");
        if !success {
            row.error = String::from_utf8_lossy(&errors).trim().to_owned();
        }
        if let Some(status) = row.status.filter(|s| *s >= 400) {
            row.error = response
                .lines()
                .find(|line| {
                    line.starts_with("HTTP/")
                        && line.split_whitespace().nth(1) == Some(status.to_string().as_str())
                })
                .map(str::to_owned)
                .unwrap_or_else(|| format!("HTTP {status}"));
        }
        row.response = response.to_owned();
        Ok(())
    })();
    if let Err(error) = outcome {
        row.timeout = error.to_ascii_lowercase().contains("timed out");
        row.error = error;
    }
    row.searchable = format!(
        "{destination}\n{request}\n{}\n{}\n{:?}\n{}\n{}\n{}\n{}",
        row.response,
        row.id,
        row.status,
        row.seconds * 1000.0,
        row.error,
        if row.timeout { "Yes" } else { "No" },
        row.length
    )
    .to_lowercase();
    row
}

fn response_preview(response: &str) -> String {
    if response.is_empty() {
        return "No response received".into();
    }
    let mut chars = response.chars();
    let mut preview: String = chars
        .by_ref()
        .take(300)
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    if chars.next().is_some() {
        preview.push('…');
    }
    preview
}

fn matches_filters(row: &RequestResult, positive: &str, negative: &str) -> bool {
    let terms = |s: &str| {
        s.lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase)
            .collect::<Vec<_>>()
    };
    terms(positive).iter().all(|s| row.searchable.contains(s))
        && !terms(negative).iter().any(|s| row.searchable.contains(s))
}

fn compare_rows(
    a: &RequestResult,
    b: &RequestResult,
    column: usize,
    variables: usize,
) -> std::cmp::Ordering {
    if column == 0 {
        return a.id.cmp(&b.id);
    }
    if column <= variables {
        return a.values[column - 1].cmp(&b.values[column - 1]);
    }
    match column - variables {
        1 => a.status.cmp(&b.status),
        2 => a.seconds.total_cmp(&b.seconds),
        3 => a.error.cmp(&b.error),
        4 => a.timeout.cmp(&b.timeout),
        5 => a.length.cmp(&b.length),
        _ => a.response.cmp(&b.response),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        time::Instant,
    };

    #[test]
    fn request_paths_compose_with_the_url() {
        for (base, target, expected) in [
            (
                "https://example.com",
                Some("/login"),
                "https://example.com/login",
            ),
            (
                "https://example.com/old?old=1",
                Some("/login?user=alice"),
                "https://example.com/login?user=alice",
            ),
            (
                "http://localhost:8080/api/",
                Some("login?next=%2Fhome"),
                "http://localhost:8080/api/login?next=%2Fhome",
            ),
            (
                "https://example.com",
                Some("/a/../login?next=%2Fhome"),
                "https://example.com/a/../login?next=%2Fhome",
            ),
            (
                "https://example.com",
                Some("//login"),
                "https://example.com//login",
            ),
            (
                "https://example.com/login?user=alice",
                None,
                "https://example.com/login?user=alice",
            ),
        ] {
            assert_eq!(destination_url(base, target).unwrap(), expected);
        }
        let request = parse_request("POST /login?user=alice HTTP/1.0\n\nbody").unwrap();
        assert_eq!(request.target.as_deref(), Some("/login?user=alice"));
        assert!(parse_request("plain body").unwrap().target.is_none());
    }

    #[test]
    fn serial_enumeration_sends_every_combination_and_records_http_results() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/ignored?old=1", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut requests = Vec::new();
            while requests.len() < 4 {
                assert!(Instant::now() < deadline, "Missing enumerated requests");
                let (mut stream, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                let received = Instant::now();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut buf = [0; 1024];
                    let n = stream.read(&mut buf).unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(head_end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&bytes[..head_end]);
                        let length: usize = head
                            .lines()
                            .find_map(|line| {
                                line.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= head_end + 4 + length {
                            break;
                        }
                    }
                }
                let status = if requests.len() == 1 {
                    "404 Not Found"
                } else {
                    "200 OK"
                };
                stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello").as_bytes()).unwrap();
                requests.push((received, String::from_utf8(bytes).unwrap()));
            }
            requests
        });
        let mut state = SecOps {
            url: endpoint,
            request: "POST /login?user=$user$ HTTP/1.0\nCookie: c=cval\nContent-Type: application/x-www-form-urlencoded\nContent-Length: 17\n\nusername=$user$&password=$password$".into(),
            ..Default::default()
        };
        state.values.insert("user".into(), "alice\nbob".into());
        state.values.insert("password".into(), "one\ntwo".into());
        state.start(&egui::Context::default()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while state.running() && Instant::now() < deadline {
            state.poll();
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!state.running());
        let requests = server.join().unwrap();
        assert_eq!(state.results.rows.len(), 4);
        for (index, (received, request)) in requests.iter().enumerate() {
            let expected = [
                ("alice", "one"),
                ("alice", "two"),
                ("bob", "one"),
                ("bob", "two"),
            ][index];
            assert!(request.starts_with(&format!("POST /login?user={} HTTP/1.0\r\n", expected.0)));
            assert!(request.ends_with(&format!("username={}&password={}", expected.0, expected.1)));
            assert!(request.contains("Cookie: c=cval\r\n"));
            assert_eq!(request.to_lowercase().matches("content-type:").count(), 1);
            assert!(
                request.contains("Content-Length: 27\r\n")
                    || request.contains("Content-Length: 25\r\n")
            );
            if index > 0 {
                assert!(
                    received.duration_since(requests[index - 1].0) >= Duration::from_millis(100)
                );
            }
            let row = &state.results.rows[index];
            assert_eq!(row.id, index as u128 + 1);
            assert_eq!(row.length, 5);
            assert!(row.response.starts_with("HTTP/1.1 "));
            assert!(row.response.ends_with("\r\n\r\nhello"));
            assert!(!row.response.contains("SKYFORGE_META:"));
            assert!(row.seconds >= 0.0 && !row.timeout);
        }
        assert_eq!(state.results.rows[1].status, Some(404));
        assert!(state.results.rows[1].error.contains("404 Not Found"));
        assert!(matches_filters(
            &state.results.rows[0],
            "ALICE\nHELLO",
            "Not Found"
        ));
        assert!(!matches_filters(
            &state.results.rows[1],
            "alice",
            "NOT FOUND"
        ));
        assert!(!matches_filters(&state.results.rows[2], "alice", ""));
    }

    #[test]
    fn timeout_and_connection_errors_are_results() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(250));
        });
        let row = send_request(
            &endpoint,
            "body",
            1,
            vec![],
            &AtomicBool::new(false),
            Duration::from_millis(50),
        );
        assert!(row.timeout);
        assert!(!row.error.is_empty());
        server.join().unwrap();
        let row = send_request(
            &endpoint,
            "body",
            2,
            vec![],
            &AtomicBool::new(false),
            Duration::from_secs(1),
        );
        assert!(row.status.is_none());
        assert!(!row.timeout && !row.error.is_empty());
    }

    #[test]
    fn interpolation_preserves_dollars_in_values_and_columns_sort_by_type() {
        assert_eq!(
            interpolate(
                "$a$ $b$ $a$ $unfinished",
                &["a".into(), "b".into()],
                &["$b$".into(), "two".into()]
            ),
            "$b$ two $b$ $unfinished"
        );
        let a = RequestResult {
            id: 2,
            values: vec!["alice".into()],
            status: Some(200),
            seconds: 0.2,
            error: "a".into(),
            timeout: false,
            length: 9,
            response: "HTTP/1.1 200 OK".into(),
            ..Default::default()
        };
        let b = RequestResult {
            id: 10,
            values: vec!["bob".into()],
            status: Some(404),
            seconds: 1.0,
            error: "z".into(),
            timeout: true,
            length: 100,
            response: "HTTP/1.1 404 Not Found".into(),
            ..Default::default()
        };
        for column in 0..8 {
            assert!(compare_rows(&a, &b, column, 1).is_lt(), "column {column}");
        }
    }

    #[test]
    fn clear_filter_restores_rows_and_response_preview_opens_full_modal() {
        fn text_rect(shape: &egui::Shape, label: &str) -> Option<egui::Rect> {
            match shape {
                egui::Shape::Text(text) if text.galley.text() == label => {
                    Some(text.visual_bounding_rect())
                }
                egui::Shape::Vec(shapes) => shapes.iter().find_map(|s| text_rect(s, label)),
                _ => None,
            }
        }
        fn frame(
            ctx: &egui::Context,
            state: &mut SecOps,
            events: Vec<egui::Event>,
        ) -> egui::FullOutput {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1360.0, 900.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    state.show(ctx);
                },
            )
        }
        fn click(ctx: &egui::Context, state: &mut SecOps, label: &str) {
            frame(ctx, state, vec![]);
            let output = frame(ctx, state, vec![]);
            let pos = output
                .shapes
                .iter()
                .find_map(|s| text_rect(&s.shape, label))
                .unwrap_or_else(|| panic!("Missing UI: {label}"))
                .center();
            frame(
                ctx,
                state,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
            frame(
                ctx,
                state,
                vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nX-Example: Original-Case\r\n\r\nHello {}\nLast line",
            "🦀".repeat(400)
        );
        let preview = response_preview(&response);
        assert!(
            preview.ends_with('…') && !preview.contains('\n') && !preview.contains("Last line")
        );
        let mut state = SecOps::default();
        state.results.positive = "hello".into();
        state.results.negative = "forbidden".into();
        state.results.rows = vec![
            RequestResult {
                id: 1,
                response: response.clone(),
                searchable: "hello".into(),
                ..Default::default()
            },
            RequestResult {
                id: 2,
                response: "Other response".into(),
                searchable: "other".into(),
                ..Default::default()
            },
        ];
        let ctx = egui::Context::default();
        frame(&ctx, &mut state, vec![]);
        let output = frame(&ctx, &mut state, vec![]);
        assert!(output
            .shapes
            .iter()
            .any(|s| text_rect(&s.shape, "1 shown · 2 completed").is_some()));
        click(&ctx, &mut state, "Clear filter");
        assert!(state.results.positive.is_empty() && state.results.negative.is_empty());
        let output = frame(&ctx, &mut state, vec![]);
        assert!(output
            .shapes
            .iter()
            .any(|s| text_rect(&s.shape, "2 shown · 2 completed").is_some()));
        click(&ctx, &mut state, &preview);
        assert_eq!(state.results.selected_response, Some(1));
        let output = frame(&ctx, &mut state, vec![]);
        assert!(output
            .shapes
            .iter()
            .any(|s| text_rect(&s.shape, "Response · Request 1").is_some()));
        assert!(output
            .shapes
            .iter()
            .any(|s| text_rect(&s.shape, &response).is_some()));
        click(&ctx, &mut state, "Close");
        assert!(state.results.selected_response.is_none());
    }

    #[test]
    fn results_resize_and_filters_apply_only_on_apply() {
        fn find_text(shape: &egui::Shape, label: &str) -> Option<egui::Rect> {
            match shape {
                egui::Shape::Text(text) if text.galley.text() == label => {
                    Some(text.visual_bounding_rect())
                }
                egui::Shape::Vec(shapes) => shapes.iter().find_map(|s| find_text(s, label)),
                _ => None,
            }
        }
        fn frame(
            ctx: &egui::Context,
            state: &mut SecOps,
            events: Vec<egui::Event>,
        ) -> egui::FullOutput {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1360.0, 900.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    state.show(ctx);
                },
            )
        }
        let ctx = egui::Context::default();
        let mut state = SecOps::default();
        frame(&ctx, &mut state, vec![]);
        let panel = || {
            egui::containers::panel::PanelState::load(&ctx, egui::Id::new("secops_results_panel"))
                .unwrap()
                .rect
        };
        let initial = panel();
        let start = egui::pos2(initial.center().x, initial.top());
        frame(
            &ctx,
            &mut state,
            vec![
                egui::Event::PointerMoved(start),
                egui::Event::PointerButton {
                    pos: start,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        let end = start - egui::vec2(0.0, 120.0);
        frame(&ctx, &mut state, vec![egui::Event::PointerMoved(end)]);
        frame(&ctx, &mut state, vec![]);
        frame(
            &ctx,
            &mut state,
            vec![egui::Event::PointerButton {
                pos: end,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        let resized = panel();
        assert!(resized.height() > initial.height() + 90.0);
        frame(&ctx, &mut state, vec![]);
        assert!((panel().height() - resized.height()).abs() < 1.0);

        state.results.filter_open = true;
        state.results.draft_positive = "hello".into();
        state.results.draft_negative = "error".into();
        frame(&ctx, &mut state, vec![]);
        let output = frame(&ctx, &mut state, vec![]);
        assert!(state.results.positive.is_empty() && state.results.negative.is_empty());
        let apply = output
            .shapes
            .iter()
            .find_map(|s| find_text(&s.shape, "Apply"))
            .unwrap()
            .center();
        frame(
            &ctx,
            &mut state,
            vec![
                egui::Event::PointerMoved(apply),
                egui::Event::PointerButton {
                    pos: apply,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        frame(
            &ctx,
            &mut state,
            vec![egui::Event::PointerButton {
                pos: apply,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(!state.results.filter_open);
        assert_eq!(state.results.positive, "hello");
        assert_eq!(state.results.negative, "error");
    }

    #[test]
    fn variables_are_unique_complete_and_ordered() {
        let state = SecOps {
            request: "username=$p3val$&password=$p4val$&again=$p3val$ $$ $bad name$ $unfinished"
                .into(),
            ..Default::default()
        };
        assert_eq!(state.variables(), ["p3val", "p4val"]);
    }

    #[test]
    fn counts_use_lines_and_all_combinations() {
        let mut state = SecOps {
            request: "$user$ $password$ $user$".into(),
            ..Default::default()
        };
        assert_eq!(state.request_count(&state.variables()), Some(0));
        state
            .values
            .insert("user".into(), "alice\r\nbob\r\n".into());
        state
            .values
            .insert("password".into(), "one\n\nthree".into());
        assert_eq!(state.payload_count("user"), 2);
        assert_eq!(state.payload_count("password"), 3);
        assert_eq!(state.request_count(&state.variables()), Some(6));
        state.request = "$user$".into();
        assert_eq!(state.request_count(&state.variables()), Some(2));
        state.request.clear();
        assert_eq!(state.request_count(&state.variables()), Some(0));
    }

    #[test]
    fn editors_and_counts_fit_the_window_and_drafts_round_trip() {
        fn text_rect(shape: &egui::Shape, label: &str) -> Option<egui::Rect> {
            match shape {
                egui::Shape::Text(text) if text.galley.text() == label => {
                    Some(text.visual_bounding_rect())
                }
                egui::Shape::Vec(shapes) => shapes.iter().find_map(|s| text_rect(s, label)),
                _ => None,
            }
        }
        let mut state = SecOps {
            url: "https://example.com/login".into(),
            request: "username=$p3val$&password=$p4val$".into(),
            ..Default::default()
        };
        state.values.insert("p3val".into(), "alice\nbob".into());
        state
            .values
            .insert("p4val".into(), "one\ntwo\nthree".into());
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        for width in [850.0, 1360.0] {
            let bounds = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 600.0));
            let mut output = None;
            for _ in 0..2 {
                output = Some(ctx.run(
                    egui::RawInput {
                        screen_rect: Some(bounds),
                        ..Default::default()
                    },
                    |ctx| {
                        state.show(ctx);
                    },
                ));
            }
            let output = output.unwrap();
            for label in [
                "URL",
                "Request payload",
                "Payload parameters",
                "Variable values",
                "Payload count: 2",
                "Request count: 6",
                "alice\nbob",
            ] {
                let rect = output
                    .shapes
                    .iter()
                    .find_map(|s| text_rect(&s.shape, label))
                    .unwrap_or_else(|| panic!("Missing SecOps UI: {label}"));
                assert!(
                    bounds.contains_rect(rect),
                    "{label} outside {width}px window: {rect:?}"
                );
            }
        }
        let draft = serde_json::to_string(&state).unwrap();
        let restored: SecOps = serde_json::from_str(&draft).unwrap();
        assert_eq!(restored.request_count(&restored.variables()), Some(6));
        assert_eq!(restored.selected.as_deref(), Some("p3val"));
        assert_eq!(restored.url, state.url);
    }
}
