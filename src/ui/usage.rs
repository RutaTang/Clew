//! Settings → Usage and cost: what the AI key has spent, by day and by
//! feature, the latest calls, the balance where the provider tells it, and
//! the prices the money is worked out at (`app::usage`).

use super::*;
use crate::UsageMsg;
use crate::app::usage::{self, Balance, PriceField, Totals};
use crate::llm;
use iced::widget::{column, row};

/// The height of the tallest bar of the days' chart.
const CHART_H: f32 = 64.0;

pub(crate) fn usage_page(app: &App) -> Element<'_, Message> {
    let mut groups = vec![balance_group(app)];
    match app.usage.entries.as_deref() {
        None => groups.push(form::group(
            tr!("Spent"),
            None,
            None,
            vec![form::item(
                tr!("This month"),
                None,
                text(tr!("Reading the ledger…"))
                    .size(ts::BODY)
                    .color(theme::dim()),
            )],
        )),
        Some(entries) => {
            let s = usage::summarize(entries, usage::now_secs());
            groups.push(spent_group(&s));
            if !s.by_purpose.is_empty() {
                groups.push(form::group(
                    tr!("This month by feature"),
                    None,
                    None,
                    s.by_purpose
                        .iter()
                        .map(|(purpose, t)| {
                            form::item(
                                usage::purpose_label(purpose),
                                Some(calls_line(t)),
                                cost_text(t),
                            )
                        })
                        .collect(),
                ));
            }
            if !s.recent.is_empty() {
                let now = usage::now_secs() as i64;
                groups.push(form::group(
                    tr!("Latest calls"),
                    None,
                    None,
                    s.recent
                        .iter()
                        .map(|e| {
                            let about = tr!(
                                "{date}, {model}: {input} in, {output} out",
                                date = super::panes::short_date(e.at as i64, now),
                                model = e.model.as_str(),
                                input = usage::tokens(e.input),
                                output = usage::tokens(e.output)
                            );
                            let cost = e.cost().map_or_else(|| "—".to_string(), usage::money);
                            form::item(
                                usage::purpose_label(&e.purpose),
                                Some(about),
                                text(cost).size(ts::BODY).color(theme::fg()),
                            )
                        })
                        .collect(),
                ));
            }
        }
    }
    groups.push(prices_group(app, &app.usage.model));
    form::page(groups)
}

/// What is left on the account: read from the provider where it tells, a
/// way to its billing page where it does not.
fn balance_group(app: &App) -> Element<'_, Message> {
    let kind = app.usage.provider.unwrap_or(llm::Provider::Anthropic);
    let provider = crate::i18n::lookup(kind.label());
    let refresh = |busy: bool| -> Element<'_, Message> {
        super::toolbar::busy_action(
            Glyph::Refresh,
            tr!("Read again"),
            Some(Message::Usage(UsageMsg::RefreshBalance)),
            busy,
        )
    };
    let row: Element<'_, Message> = if !app.usage.tells_balance {
        let about = match kind {
            llm::Provider::Custom => tr!("This endpoint does not tell its balance.").to_string(),
            _ => tr!(
                "{provider} does not tell an API key its balance.",
                provider = provider
            ),
        };
        let control: Element<'_, Message> = match usage::billing_page(kind) {
            Some(_) => form::action(
                tr!("Open billing page"),
                Some(Message::Usage(UsageMsg::OpenBilling)),
            )
            .into(),
            None => space().width(0).into(),
        };
        form::item(tr!("Balance"), Some(about), control)
    } else if !app.llm_available {
        form::item(
            tr!("Balance"),
            Some(tr!("Add an API key to read it.").into()),
            space().width(0),
        )
    } else {
        let told = tr!(
            "As {provider} tells it. Reading it uses no tokens.",
            provider = provider
        );
        match &app.usage.balance {
            Balance::Read(amounts, _) => form::item(
                tr!("Balance"),
                Some(told),
                row![
                    text(
                        amounts
                            .iter()
                            .map(|(c, a)| amount(c, a))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                    .size(ts::BASE)
                    .font(crate::fonts::UI_MEDIUM)
                    .color(theme::fg()),
                    refresh(false),
                ]
                .spacing(8)
                .align_y(iced::Center),
            ),
            Balance::Failed(e) => form::item(
                tr!("Balance"),
                Some(tr!("Could not read it: {error}", error = e.as_str())),
                refresh(false),
            ),
            Balance::Reading | Balance::Unread | Balance::NotTold => form::item(
                tr!("Balance"),
                Some(told),
                row![
                    text(tr!("Reading the balance…"))
                        .size(ts::BODY)
                        .color(theme::dim()),
                    refresh(true),
                ]
                .spacing(8)
                .align_y(iced::Center),
            ),
        }
    };
    form::group(tr!("Account"), None, None, vec![row])
}

/// An amount in its currency, as a reader writes it: ¥110.00, $3.10.
fn amount(currency: &str, value: &str) -> String {
    match currency {
        "CNY" => format!("¥{value}"),
        "USD" => format!("${value}"),
        other => format!("{value} {other}"),
    }
}

/// Today, this month, and the days before: what each spent.
fn spent_group<'a>(s: &usage::Summary) -> Element<'a, Message> {
    let mut rows = vec![
        form::item(
            tr!("Today"),
            Some(calls_line(&s.today)),
            cost_text(&s.today),
        ),
        form::item(
            tr!("This month"),
            Some(calls_line(&s.month)),
            cost_text(&s.month),
        ),
    ];
    // The last 30 days, a bar a day, today last: by cost where every call
    // has a price, else by tokens.
    let priced = s.days.iter().all(|d| d.unpriced == 0);
    let measure = |d: &Totals| {
        if priced { d.cost } else { d.tokens() as f64 }
    };
    let top = s.days.iter().map(measure).fold(0.0_f64, f64::max);
    let bars = iced::widget::Row::with_children(s.days.iter().map(|d| {
        let h = if top > 0.0 {
            (measure(d) / top * f64::from(CHART_H)) as f32
        } else {
            0.0
        };
        let used = d.calls > 0;
        let bar = container(space().height(h.max(if used { 2.0 } else { 1.0 })))
            .width(Fill)
            .style(move |_: &iced::Theme| container::Style {
                background: Some(
                    if used {
                        theme::accent()
                    } else {
                        theme::border()
                    }
                    .into(),
                ),
                border: iced::Border {
                    radius: 1.5.into(),
                    ..iced::Border::default()
                },
                ..container::Style::default()
            });
        container(bar)
            .height(CHART_H)
            .width(Fill)
            .align_y(iced::alignment::Vertical::Bottom)
            .into()
    }))
    .spacing(3)
    .width(Fill);
    rows.push(form::row_of(
        column![
            row![
                text(tr!("Last 30 days"))
                    .size(ts::SMALL)
                    .color(theme::dim()),
                space().width(Fill),
                text(if priced {
                    tr!("By cost")
                } else {
                    tr!("By tokens")
                })
                .size(ts::SMALL)
                .color(theme::dim()),
            ],
            bars,
        ]
        .spacing(8),
    ));
    let unpriced = s.month.unpriced;
    form::group(
        tr!("Spent"),
        Some(if unpriced > 0 {
            trn!(
                "Estimated from each call's tokens at the prices below; 1 call this month has no price.",
                "Estimated from each call's tokens at the prices below; {n} calls this month have no price.",
                unpriced as usize
            )
        } else {
            tr!("Estimated from each call's tokens at the prices below; your provider's bill is what counts.")
                .to_string()
        }),
        None,
        rows,
    )
}

/// "12 calls, 48k tokens".
fn calls_line(t: &Totals) -> String {
    trn!(
        "1 call, {tokens} tokens",
        "{n} calls, {tokens} tokens",
        t.calls as usize,
        tokens = usage::tokens(t.tokens())
    )
}

/// What `t` cost, or its tokens where none of it has a price.
fn cost_text<'a>(t: &Totals) -> Element<'a, Message> {
    let said = if t.calls > 0 && t.unpriced == t.calls {
        "—".to_string()
    } else {
        usage::money(t.cost)
    };
    text(said)
        .size(ts::BASE)
        .font(crate::fonts::UI_MEDIUM)
        .color(theme::fg())
        .into()
}

/// The model in use's prices: Clew's, or the reader's own.
fn prices_group<'a>(app: &'a App, model: &str) -> Element<'a, Message> {
    let shown = app.usage.price_draft.clone().unwrap_or_else(|| {
        usage::price_of(model).map_or_else(Default::default, |p| {
            [p.input, p.cached, p.output].map(usage::price_text)
        })
    });
    let field = |label: &str, which: PriceField, value: String| {
        form::field(
            label.to_string(),
            None,
            form::input("0", &value)
                .on_input(move |v| Message::Usage(UsageMsg::Price(which, v)))
                .width(Length::Fixed(120.0)),
        )
    };
    let [input, cached, output] = shown;
    let mut rows = vec![
        field(tr!("Input"), PriceField::Input, input),
        field(tr!("Cached input"), PriceField::Cached, cached),
        field(tr!("Output"), PriceField::Output, output),
    ];
    if usage::own_price(model).is_some() {
        rows.push(form::item(
            tr!("Your own prices"),
            usage::builtin_price(model)
                .is_some()
                .then(|| tr!("Clew has its own for this model.").to_string()),
            form::action(
                tr!("Use Clew's prices"),
                Some(Message::Usage(UsageMsg::ResetPrice)),
            ),
        ));
    }
    let about = if model.is_empty() {
        tr!("Set a model on the Language model page first.").to_string()
    } else if usage::price_of(model).is_none() {
        tr!(
            "Clew has no price for {model}: set one to see what calls cost. In US dollars per million tokens.",
            model = model
        )
    } else {
        tr!(
            "For {model}, in US dollars per million tokens. Clew's own were looked up in {date}; prices change, so check your provider's page.",
            model = model,
            date = usage::PRICES_AS_OF
        )
    };
    form::group(tr!("Prices"), Some(about), None, rows)
}
