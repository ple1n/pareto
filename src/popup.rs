use std::{
    io::{BufRead, BufReader},
    os::unix::net::UnixStream,
    path::Path,
    sync::Arc,
    thread,
    time::Duration,
};

use anyhow::Result;
use futures::SinkExt;
use iced::{
    Border, Color, Element, Event, Fill, Length, Subscription, Task, Theme, event, keyboard,
    widget::{column, container, scrollable, text, text_input},
};
use iced_exwlshell::{
    actions::RegionActionCB,
    layershell::application,
    reexport::{Anchor, BlurOption, KeyboardInteractivity, Layer, LayerSize},
    settings::LayerShellSettings,
    to_layer_message,
};

use crate::dictionary::{Dictionary, Entry};

const WIDTH: i32 = 600;
const HEIGHT: i32 = 520;

pub fn run(data_dir: &Path) -> Result<()> {
    let dictionary = Arc::new(Dictionary::open(data_dir)?);

    application(
        move || State::new(Arc::clone(&dictionary)),
        namespace,
        update,
        view,
    )
    .layer_settings(LayerShellSettings {
        size: LayerSize::px(WIDTH as u32, HEIGHT as u32),
        anchor: Anchor::empty(),
        layer: Layer::Top,
        keyboard_interactivity: KeyboardInteractivity::OnDemand,
        blur_option: BlurOption::FullRegion,
        events_transparent: true,
        ..Default::default()
    })
    .subscription(subscription)
    .style(style)
    .run()?;

    Ok(())
}

struct State {
    dictionary: Arc<Dictionary>,
    query: String,
    entries: Vec<Entry>,
    error: Option<String>,
    visible: bool,
}

impl State {
    fn new(dictionary: Arc<Dictionary>) -> Self {
        Self {
            dictionary,
            query: String::new(),
            entries: Vec::new(),
            error: None,
            visible: true,
        }
    }
}

#[to_layer_message]
#[derive(Debug, Clone)]
enum Message {
    Exit,
    QueryChanged(String),
    LookupFinished(std::result::Result<Vec<Entry>, String>),
    SelectionRead(Option<String>),
    Toggle,
    IcedEvent(Event),
}

fn update(state: &mut State, message: Message) -> Task<Message> {
    match message {
        Message::Exit => iced::exit(),
        Message::QueryChanged(query) => {
            state.query = query;
            lookup_task(state)
        }
        Message::LookupFinished(Ok(entries)) => {
            state.entries = entries;
            state.error = None;
            Task::none()
        }
        Message::LookupFinished(Err(error)) => {
            state.entries.clear();
            state.error = Some(error);
            Task::none()
        }
        Message::SelectionRead(selection) => {
            if let Some(selection) = selection {
                let selection = selection.trim().to_owned();
                if !selection.is_empty() && selection.len() < 20 {
                    state.query = selection;
                    return lookup_task(state);
                }
            }
            Task::none()
        }
        Message::Toggle => {
            state.visible = !state.visible;
            let visible = state.visible;
            let layer = if visible {
                Layer::Top
            } else {
                Layer::Background
            };
            let keyboard = if visible {
                KeyboardInteractivity::OnDemand
            } else {
                KeyboardInteractivity::None
            };
            let tasks = vec![
                Task::done(Message::LayerChange(layer)),
                Task::done(Message::KeyboardInteractivityChange(keyboard)),
                Task::done(Message::SetInputRegion(input_region(visible))),
            ];
            let mut tasks = tasks;
            tasks.push(iced::clipboard::read_primary().map(Message::SelectionRead));
            Task::batch(tasks)
        }
        Message::IcedEvent(Event::Window(iced::window::Event::Opened { .. })) => Task::batch([
            Task::done(Message::SetInputRegion(input_region(true))),
            iced::clipboard::read_primary().map(Message::SelectionRead),
        ]),
        Message::IcedEvent(_) => Task::none(),
        _ => Task::none(),
    }
}

fn lookup_task(state: &mut State) -> Task<Message> {
    if state.query.trim().is_empty() {
        state.entries.clear();
        state.error = None;
        return Task::none();
    }

    let dictionary = Arc::clone(&state.dictionary);
    let query = state.query.clone();
    Task::perform(
        async move {
            dictionary
                .search(&query, 5)
                .map_err(|error| error.to_string())
        },
        Message::LookupFinished,
    )
}

fn subscription(_: &State) -> Subscription<Message> {
    let shortcut = keyboard::listen().filter_map(|event| {
        let keyboard::Event::KeyPressed { key, modifiers, .. } = event else {
            return None;
        };
        if modifiers.command() && key.as_ref() == keyboard::Key::Character("q") {
            Some(Message::Exit)
        } else {
            None
        }
    });

    Subscription::batch([
        event::listen().map(Message::IcedEvent),
        shortcut,
        gesture_events(),
    ])
}

fn gesture_events() -> Subscription<Message> {
    Subscription::run(|| {
        iced::stream::channel(
            10,
            |mut output: futures::channel::mpsc::Sender<Message>| async move {
                thread::spawn(move || {
                    let mut reported_unavailable = false;
                    loop {
                        let socket =
                            format!("/run/pareto/gesture-{}.sock", unsafe { libc::getuid() });
                        match UnixStream::connect(&socket) {
                            Ok(stream) => {
                                reported_unavailable = false;
                                let reader = BufReader::new(stream);
                                for line in reader.lines() {
                                    match line {
                                        Ok(line) if line == "toggle" => {
                                            if futures::executor::block_on(
                                                output.send(Message::Toggle),
                                            )
                                            .is_err()
                                            {
                                                return;
                                            }
                                        }
                                        Ok(_) => {}
                                        Err(error) => {
                                            eprintln!("gesture socket read failed: {error}");
                                            break;
                                        }
                                    }
                                }
                            }
                            Err(error) => {
                                if !reported_unavailable {
                                    eprintln!("gesture daemon is not available: {error}");
                                    reported_unavailable = true;
                                }
                            }
                        }
                        thread::sleep(Duration::from_millis(500));
                    }
                });
                futures::future::pending::<()>().await;
            },
        )
    })
}

fn input_region(visible: bool) -> RegionActionCB {
    RegionActionCB::new(move |region| {
        if visible {
            let radius = 22_i32.min(WIDTH / 2).min(HEIGHT / 2);
            for y in 0..HEIGHT {
                let inset = rounded_corner_inset(y, WIDTH, HEIGHT, radius);
                let span = WIDTH - 2 * inset;
                if span > 0 {
                    let _ = region.add(inset, y, span, 1);
                }
            }
        }
    })
}

fn rounded_corner_inset(y: i32, width: i32, height: i32, radius: i32) -> i32 {
    if radius == 0 || (y >= radius && y < height - radius) {
        return 0;
    }

    let row_center = if y < radius {
        y as f64 + 0.5
    } else {
        height as f64 - y as f64 - 0.5
    };
    let distance = radius as f64 - row_center;
    let remaining = (radius as f64).powi(2) - distance.powi(2);
    (radius as f64 - remaining.max(0.0).sqrt())
        .ceil()
        .clamp(0.0, (width / 2) as f64) as i32
}

fn view(state: &State) -> Element<'_, Message> {
    let results = if let Some(error) = &state.error {
        column![text(error).size(14)]
    } else {
        state.entries.iter().fold(column![], |column, entry| {
            column.push(
                container(
                    column![
                        text(format!("{} · {}", entry.word, entry.dictionary)).size(17),
                        text(&entry.definition).size(14),
                    ]
                    .spacing(6),
                )
                .padding(12)
                .width(Fill),
            )
        })
    };

    container(
        column![
            text_input("Select text or type a word", &state.query)
                .on_input(Message::QueryChanged)
                .padding(12)
                .size(16)
                .style(|_, status| {
                    let border_color =
                        if matches!(status, iced::widget::text_input::Status::Focused { .. }) {
                            Color::from_rgba(0.55, 0.74, 1.0, 0.72)
                        } else {
                            Color::from_rgba(0.72, 0.82, 1.0, 0.2)
                        };

                    iced::widget::text_input::Style {
                        background: Color::from_rgba(0.08, 0.12, 0.2, 0.3).into(),
                        border: Border {
                            color: border_color,
                            width: 1.0,
                            radius: 12.0.into(),
                        },
                        placeholder: Color::from_rgb(0.55, 0.62, 0.73),
                        value: Color::WHITE,
                        icon: Color::WHITE,
                        selection: Color::from_rgb(0.28, 0.48, 0.78),
                    }
                }),
            scrollable(results).height(Length::Fill),
        ]
        .spacing(14)
        .padding(20),
    )
    .width(Fill)
    .height(Fill)
    .padding(22)
    .style(|_| {
        container::Style::default()
            .background(Color::from_rgba(0.035, 0.055, 0.09, 0.05))
            .border(Border {
                color: Color::from_rgba(0.72, 0.82, 1.0, 0.25),
                width: 1.0,
                radius: 22.0.into(),
            })
    })
    .into()
}

fn style(_: &State, theme: &Theme) -> iced::theme::Style {
    iced::theme::Style {
        background_color: Color::TRANSPARENT,
        text_color: theme.palette().text,
    }
}

fn namespace() -> String {
    String::from("pareto dictionary popup")
}
