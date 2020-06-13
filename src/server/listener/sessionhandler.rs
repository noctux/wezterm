use crate::mux::renderable::{RenderableDimensions, StableCursorPosition};
use crate::mux::tab::{Tab, TabId};
use crate::mux::Mux;
use crate::server::codec::*;
use crate::server::listener::PKI;
use crate::server::pollable::*;
use anyhow::anyhow;
use portable_pty::PtySize;
use promise::spawn::spawn_into_main_thread;
use rangeset::RangeSet;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use url::Url;
use wezterm_term::terminal::Clipboard;
use wezterm_term::StableRowIndex;

#[derive(Default, Debug)]
struct PerTab {
    cursor_position: StableCursorPosition,
    title: String,
    working_dir: Option<Url>,
    dimensions: RenderableDimensions,
    dirty_lines: RangeSet<StableRowIndex>,
    mouse_grabbed: bool,
}

impl PerTab {
    fn compute_changes(
        &mut self,
        tab: &Rc<dyn Tab>,
        force_with_input_serial: Option<InputSerial>,
    ) -> Option<GetTabRenderChangesResponse> {
        let mut changed = false;
        let mouse_grabbed = tab.is_mouse_grabbed();
        if mouse_grabbed != self.mouse_grabbed {
            changed = true;
        }

        let dims = tab.renderer().get_dimensions();
        if dims != self.dimensions {
            changed = true;
        }

        let cursor_position = tab.renderer().get_cursor_position();
        if cursor_position != self.cursor_position {
            changed = true;
        }

        let title = tab.get_title();
        if title != self.title {
            changed = true;
        }

        let working_dir = tab.get_current_working_dir();
        if working_dir != self.working_dir {
            changed = true;
        }

        let mut all_dirty_lines = tab
            .renderer()
            .get_dirty_lines(0..dims.physical_top + dims.viewport_rows as StableRowIndex);
        let dirty_delta = all_dirty_lines.difference(&self.dirty_lines);
        if !dirty_delta.is_empty() {
            changed = true;
        }

        if !changed && !force_with_input_serial.is_some() {
            return None;
        }

        // Figure out what we're going to send as dirty lines vs bonus lines
        let viewport_range =
            dims.physical_top..dims.physical_top + dims.viewport_rows as StableRowIndex;

        let (first_line, lines) = tab.renderer().get_lines(viewport_range);
        let mut bonus_lines = lines
            .into_iter()
            .enumerate()
            .map(|(idx, line)| {
                let stable_row = first_line + idx as StableRowIndex;
                all_dirty_lines.remove(stable_row);
                (stable_row, line)
            })
            .collect::<Vec<_>>();

        // Always send the cursor's row, as that tends to the busiest and we don't
        // have a sequencing concept for our idea of the remote state.
        let (cursor_line, lines) = tab
            .renderer()
            .get_lines(cursor_position.y..cursor_position.y + 1);
        bonus_lines.push((cursor_line, lines[0].clone()));

        self.cursor_position = cursor_position;
        self.title = title.clone();
        self.working_dir = working_dir.clone();
        self.dimensions = dims;
        self.dirty_lines = all_dirty_lines;
        self.mouse_grabbed = mouse_grabbed;

        let dirty_lines = dirty_delta.iter().cloned().collect();
        let bonus_lines = bonus_lines.into();
        Some(GetTabRenderChangesResponse {
            tab_id: tab.tab_id(),
            mouse_grabbed,
            dirty_lines,
            dimensions: dims,
            cursor_position,
            title,
            bonus_lines,
            working_dir: working_dir.map(Into::into),
            input_serial: force_with_input_serial,
        })
    }

    fn mark_clean(&mut self, stable_row: StableRowIndex) {
        self.dirty_lines.remove(stable_row);
    }
}

fn maybe_push_tab_changes(
    tab: &Rc<dyn Tab>,
    sender: PollableSender<DecodedPdu>,
    per_tab: Arc<Mutex<PerTab>>,
) -> anyhow::Result<()> {
    let mut per_tab = per_tab.lock().unwrap();
    if let Some(resp) = per_tab.compute_changes(tab, None) {
        sender.send(DecodedPdu {
            pdu: Pdu::GetTabRenderChangesResponse(resp),
            serial: 0,
        })?;
    }
    Ok(())
}

pub struct SessionHandler {
    to_write_tx: PollableSender<DecodedPdu>,
    per_tab: HashMap<TabId, Arc<Mutex<PerTab>>>,
}

impl SessionHandler {
    pub fn new(to_write_tx: PollableSender<DecodedPdu>) -> Self {
        Self {
            to_write_tx,
            per_tab: HashMap::new(),
        }
    }
    fn per_tab(&mut self, tab_id: TabId) -> Arc<Mutex<PerTab>> {
        Arc::clone(
            self.per_tab
                .entry(tab_id)
                .or_insert_with(|| Arc::new(Mutex::new(PerTab::default()))),
        )
    }

    pub fn schedule_tab_push(&mut self, tab_id: TabId) {
        let sender = self.to_write_tx.clone();
        let per_tab = self.per_tab(tab_id);
        spawn_into_main_thread(async move {
            let mux = Mux::get().unwrap();
            let tab = mux
                .get_tab(tab_id)
                .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
            maybe_push_tab_changes(&tab, sender, per_tab)?;
            Ok::<(), anyhow::Error>(())
        });
    }

    pub fn process_one(&mut self, decoded: DecodedPdu) {
        let start = Instant::now();
        let sender = self.to_write_tx.clone();
        let serial = decoded.serial;

        let send_response = move |result: anyhow::Result<Pdu>| {
            let pdu = match result {
                Ok(pdu) => pdu,
                Err(err) => Pdu::ErrorResponse(ErrorResponse {
                    reason: format!("Error: {}", err),
                }),
            };
            log::trace!("{} processing time {:?}", serial, start.elapsed());
            sender.send(DecodedPdu { pdu, serial }).ok();
        };

        fn catch<F, SND>(f: F, send_response: SND)
        where
            F: FnOnce() -> anyhow::Result<Pdu>,
            SND: Fn(anyhow::Result<Pdu>),
        {
            send_response(f());
        }

        match decoded.pdu {
            Pdu::Ping(Ping {}) => send_response(Ok(Pdu::Pong(Pong {}))),
            Pdu::ListTabs(ListTabs {}) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get().unwrap();
                            let mut tabs = vec![];
                            for window_id in mux.iter_windows().into_iter() {
                                let window = mux.get_window(window_id).unwrap();
                                for tab in window.iter() {
                                    let dims = tab.renderer().get_dimensions();
                                    let working_dir = tab.get_current_working_dir();
                                    tabs.push(WindowAndTabEntry {
                                        window_id,
                                        tab_id: tab.tab_id(),
                                        title: tab.get_title(),
                                        size: PtySize {
                                            cols: dims.cols as u16,
                                            rows: dims.viewport_rows as u16,
                                            pixel_height: 0,
                                            pixel_width: 0,
                                        },
                                        working_dir: working_dir.map(Into::into),
                                    });
                                }
                            }
                            log::error!("ListTabs {:#?}", tabs);
                            Ok(Pdu::ListTabsResponse(ListTabsResponse { tabs }))
                        },
                        send_response,
                    )
                });
            }

            Pdu::WriteToTab(WriteToTab { tab_id, data }) => {
                let sender = self.to_write_tx.clone();
                let per_tab = self.per_tab(tab_id);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get().unwrap();
                            let tab = mux
                                .get_tab(tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                            tab.writer().write_all(&data)?;
                            maybe_push_tab_changes(&tab, sender, per_tab)?;
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    );
                });
            }
            Pdu::SendPaste(SendPaste { tab_id, data }) => {
                let sender = self.to_write_tx.clone();
                let per_tab = self.per_tab(tab_id);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get().unwrap();
                            let tab = mux
                                .get_tab(tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                            tab.send_paste(&data)?;
                            maybe_push_tab_changes(&tab, sender, per_tab)?;
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                });
            }

            Pdu::SearchTabScrollbackRequest(SearchTabScrollbackRequest { tab_id, pattern }) => {
                use crate::mux::tab::Pattern;

                async fn do_search(tab_id: TabId, pattern: Pattern) -> anyhow::Result<Pdu> {
                    let mux = Mux::get().unwrap();
                    let tab = mux
                        .get_tab(tab_id)
                        .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;

                    tab.search(pattern).await.map(|results| {
                        Pdu::SearchTabScrollbackResponse(SearchTabScrollbackResponse { results })
                    })
                }

                spawn_into_main_thread(async move {
                    promise::spawn::spawn(async move {
                        let result = do_search(tab_id, pattern).await;
                        send_response(result);
                    });
                });
            }

            Pdu::Resize(Resize { tab_id, size }) => {
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get().unwrap();
                            let tab = mux
                                .get_tab(tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                            tab.resize(size)?;
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                });
            }

            Pdu::SendKeyDown(SendKeyDown {
                tab_id,
                event,
                input_serial,
            }) => {
                let sender = self.to_write_tx.clone();
                let per_tab = self.per_tab(tab_id);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get().unwrap();
                            let tab = mux
                                .get_tab(tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                            tab.key_down(event.key, event.modifiers)?;

                            // For a key press, we want to always send back the
                            // cursor position so that the predictive echo doesn't
                            // leave the cursor in the wrong place
                            let mut per_tab = per_tab.lock().unwrap();
                            if let Some(resp) = per_tab.compute_changes(&tab, Some(input_serial)) {
                                sender.send(DecodedPdu {
                                    pdu: Pdu::GetTabRenderChangesResponse(resp),
                                    serial: 0,
                                })?;
                            }
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                });
            }
            Pdu::SendMouseEvent(SendMouseEvent { tab_id, event }) => {
                let sender = self.to_write_tx.clone();
                let per_tab = self.per_tab(tab_id);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get().unwrap();
                            let tab = mux
                                .get_tab(tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                            tab.mouse_event(event)?;
                            maybe_push_tab_changes(&tab, sender, per_tab)?;
                            Ok(Pdu::UnitResponse(UnitResponse {}))
                        },
                        send_response,
                    )
                });
            }

            Pdu::Spawn(spawn) => {
                let sender = self.to_write_tx.clone();
                spawn_into_main_thread(async move {
                    schedule_domain_spawn(spawn, sender, send_response);
                });
            }

            Pdu::GetTabRenderChanges(GetTabRenderChanges { tab_id, .. }) => {
                let sender = self.to_write_tx.clone();
                let per_tab = self.per_tab(tab_id);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get().unwrap();
                            let tab_alive = match mux.get_tab(tab_id) {
                                Some(tab) => {
                                    maybe_push_tab_changes(&tab, sender, per_tab)?;
                                    true
                                }
                                None => false,
                            };
                            Ok(Pdu::TabLivenessResponse(TabLivenessResponse {
                                tab_id,
                                tab_alive,
                            }))
                        },
                        send_response,
                    )
                });
            }

            Pdu::GetLines(GetLines { tab_id, lines }) => {
                let per_tab = self.per_tab(tab_id);
                spawn_into_main_thread(async move {
                    catch(
                        move || {
                            let mux = Mux::get().unwrap();
                            let tab = mux
                                .get_tab(tab_id)
                                .ok_or_else(|| anyhow!("no such tab {}", tab_id))?;
                            let mut renderer = tab.renderer();

                            let mut lines_and_indices = vec![];
                            let mut per_tab = per_tab.lock().unwrap();

                            for range in lines {
                                let (first_row, lines) = renderer.get_lines(range);
                                for (idx, line) in lines.into_iter().enumerate() {
                                    let stable_row = first_row + idx as StableRowIndex;
                                    per_tab.mark_clean(stable_row);
                                    lines_and_indices.push((stable_row, line));
                                }
                            }
                            Ok(Pdu::GetLinesResponse(GetLinesResponse {
                                tab_id,
                                lines: lines_and_indices.into(),
                            }))
                        },
                        send_response,
                    )
                });
            }

            Pdu::GetCodecVersion(_) => {
                send_response(Ok(Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
                    codec_vers: CODEC_VERSION,
                    version_string: crate::wezterm_version().to_owned(),
                })))
            }

            Pdu::GetTlsCreds(_) => {
                catch(
                    move || {
                        let client_cert_pem = PKI.generate_client_cert()?;
                        let ca_cert_pem = PKI.ca_pem_string()?;
                        Ok(Pdu::GetTlsCredsResponse(GetTlsCredsResponse {
                            client_cert_pem,
                            ca_cert_pem,
                        }))
                    },
                    send_response,
                );
            }

            Pdu::Invalid { .. } => send_response(Err(anyhow!("invalid PDU {:?}", decoded.pdu))),
            Pdu::Pong { .. }
            | Pdu::ListTabsResponse { .. }
            | Pdu::SetClipboard { .. }
            | Pdu::SpawnResponse { .. }
            | Pdu::GetTabRenderChangesResponse { .. }
            | Pdu::UnitResponse { .. }
            | Pdu::TabLivenessResponse { .. }
            | Pdu::SearchTabScrollbackResponse { .. }
            | Pdu::GetLinesResponse { .. }
            | Pdu::GetCodecVersionResponse { .. }
            | Pdu::GetTlsCredsResponse { .. }
            | Pdu::ErrorResponse { .. } => {
                send_response(Err(anyhow!("expected a request, got {:?}", decoded.pdu)))
            }
        }
    }
}

// Dancing around a little bit here; we can't directly spawn_into_main_thread the domain_spawn
// function below because the compiler thinks that all of its locals then need to be Send.
// We need to shimmy through this helper to break that aspect of the compiler flow
// analysis and allow things to compile.
fn schedule_domain_spawn<SND>(spawn: Spawn, sender: PollableSender<DecodedPdu>, send_response: SND)
where
    SND: Fn(anyhow::Result<Pdu>) + 'static,
{
    promise::spawn::spawn(async move { send_response(domain_spawn(spawn, sender).await) });
}

struct RemoteClipboard {
    sender: PollableSender<DecodedPdu>,
    tab_id: TabId,
}

impl Clipboard for RemoteClipboard {
    fn get_contents(&self) -> anyhow::Result<String> {
        Ok("".to_owned())
    }

    fn set_contents(&self, clipboard: Option<String>) -> anyhow::Result<()> {
        self.sender.send(DecodedPdu {
            serial: 0,
            pdu: Pdu::SetClipboard(SetClipboard {
                tab_id: self.tab_id,
                clipboard,
            }),
        })?;
        Ok(())
    }
}

async fn domain_spawn(spawn: Spawn, sender: PollableSender<DecodedPdu>) -> anyhow::Result<Pdu> {
    let mux = Mux::get().unwrap();
    let domain = mux
        .get_domain(spawn.domain_id)
        .ok_or_else(|| anyhow!("domain {} not found on this server", spawn.domain_id))?;

    let window_id = if let Some(window_id) = spawn.window_id {
        mux.get_window_mut(window_id)
            .ok_or_else(|| anyhow!("window_id {} not found on this server", window_id))?;
        window_id
    } else {
        mux.new_empty_window()
    };

    let tab = domain
        .spawn(spawn.size, spawn.command, spawn.command_dir, window_id)
        .await?;

    let clip: Arc<dyn Clipboard> = Arc::new(RemoteClipboard {
        tab_id: tab.tab_id(),
        sender,
    });
    tab.set_clipboard(&clip);

    Ok::<Pdu, anyhow::Error>(Pdu::SpawnResponse(SpawnResponse {
        tab_id: tab.tab_id(),
        window_id,
    }))
}
