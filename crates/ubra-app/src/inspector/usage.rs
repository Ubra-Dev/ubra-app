//! Inspector Usage surface: the account-level usage report (local Claude Code /
//! Codex transcripts plus billed Cursor usage, including remote hosts),
//! adapted to the narrow sidebar.
//!
//! Data comes from the shared `UsageSnapshot` watch feed, not from
//! per-session RPC accounting.
use super::sidebar::{message, panel};
use super::*;
use crate::surface_shell::usage_chart::{self, ChartSample};
use crate::usage::RemoteUsageStatus;
use crate::usage::UsageFormat;
use crate::usage::dashboard::{UsageCompare, UsageHistory, date_label, hour_label};
use gpui::{Bounds, Pixels, Rgba, canvas};
const PROVIDERS: [&str; 3] = ["Claude Code", "Codex", "Cursor"];
const SERIES_LABELS: [&str; 3] = ["Claude", "Codex", "Cursor"];

impl WorkbenchInspector {
    pub(crate) fn set_account_usage(
        &mut self,
        usage: crate::usage::UsageSnapshot,
        cx: &mut Context<Self>,
    ) {
        if self.account_usage != usage {
            self.account_usage_cache.borrow_mut().take();
        }
        if self
            .account_usage_host
            .as_deref()
            .is_some_and(|id| !id.is_empty() && !usage.remote.iter().any(|host| host.host == id))
        {
            self.account_usage_host = None;
        }
        self.account_usage = usage;
        cx.notify();
    }

    fn account_usage_now(&self) -> i64 {
        self.account_usage
            .remote
            .iter()
            .filter_map(|host| host.data.as_ref().map(|data| data.collected_at))
            .fold(self.account_usage.updated_at, i64::max)
    }

    /// Canonical usage-provider indexes in recency order, for presentation.
    /// Accounting keeps the canonical index everywhere; only the layout order
    /// follows recency. Mirrors the Settings usage page.
    fn account_usage_provider_order(&self) -> [usize; 3] {
        let mru = self
            .runtime
            .store
            .read()
            .expect("session store lock poisoned")
            .preferences()
            .recent_agents
            .clone();
        crate::agent_catalog::usage_provider_order(&mru)
    }

    fn account_usage_report_cached(&self) -> (UsageCompare, [Vec<ChartSample>; 3]) {
        let remote: Vec<(String, i64)> = self
            .account_usage
            .remote
            .iter()
            .filter_map(|remote| {
                remote
                    .data
                    .as_ref()
                    .map(|data| (data.source_id.clone(), data.collected_at))
            })
            .collect();
        if let Some(hit) = self
            .account_usage_cache
            .borrow()
            .as_ref()
            .and_then(|cached| {
                (cached.updated_at == self.account_usage.updated_at
                    && cached.days == self.account_usage_days
                    && cached.host.as_deref() == self.account_usage_host.as_deref()
                    && cached.tokens == self.account_usage_tokens
                    && cached.remote == remote)
                    .then(|| (cached.compare.clone(), cached.providers.clone()))
            })
        {
            return hit;
        }
        let history = self
            .account_usage
            .history_for_source(self.account_usage_host.as_deref());
        let now = self.account_usage_now();
        let compare = history.compare(now, self.account_usage_days);
        let providers = self.account_chart_samples(&history, now);
        *self.account_usage_cache.borrow_mut() = Some(AccountUsageCache {
            updated_at: self.account_usage.updated_at,
            days: self.account_usage_days,
            host: self.account_usage_host.clone(),
            tokens: self.account_usage_tokens,
            remote,
            compare: compare.clone(),
            providers: providers.clone(),
        });
        (compare, providers)
    }

    fn account_chart_samples(&self, history: &UsageHistory, now: i64) -> [Vec<ChartSample>; 3] {
        let tokens = self.account_usage_tokens;
        let mut providers = [Vec::new(), Vec::new(), Vec::new()];
        for (hour, details) in history.hourly_provider_totals(now, 90) {
            for (index, detail) in details.into_iter().enumerate() {
                providers[index].push(ChartSample {
                    time: hour,
                    value: if tokens {
                        detail.totals().total_tokens() as f64
                    } else {
                        detail.tokens.c
                    },
                });
            }
        }
        providers
    }

    fn account_chart_series(
        &self,
        providers: &[Vec<ChartSample>; 3],
        window_days: f32,
        end: i64,
    ) -> Vec<Vec<ChartSample>> {
        match self.account_usage_chart_split {
            None => vec![usage_chart::displayed_series(
                &usage_chart::combined_series(providers),
                window_days,
                end,
            )],
            Some(visible) => self
                .account_usage_provider_order()
                .into_iter()
                .filter(|&index| visible[index])
                .map(|index| usage_chart::displayed_series(&providers[index], window_days, end))
                .collect(),
        }
    }

    pub(super) fn render_usage(
        &mut self,
        colors: SemanticColors,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.remote_usage_viewer
            .set_viewing(self.visible && self.workspace_selected == Some(WorkspaceSurface::Usage));
        if self.account_usage.updated_at == 0 {
            return panel("usage-panel", colors)
                .child(message("Reading local usage…", colors))
                .child(message(
                    "Preparing costs and token history from local Claude Code and Codex transcripts, plus billed Cursor usage when signed in.",
                    colors,
                ))
                .into_any_element();
        }
        let (compare, chart_providers) = self.account_usage_report_cached();
        let report = &compare.current;
        let total = report.total.totals();
        let tokens = self.account_usage_tokens;

        let mut view = panel("usage-panel", colors).child(section_label("Usage", colors));

        view = view.child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(4.0))
                .px(px(2.0))
                .children([1, 7, 30, 90].map(|days| {
                    let selected = self.account_usage_days == days;
                    div()
                        .id(SharedString::from(format!("inspector-usage-range-{days}")))
                        .px(px(8.0))
                        .py(px(3.0))
                        .rounded(px(6.0))
                        .bg(if selected {
                            colors.primary.alpha(0.12)
                        } else {
                            colors.primary.alpha(0.05)
                        })
                        .cursor_pointer()
                        .text_size(px(11.0))
                        .text_color(if selected {
                            colors.primary
                        } else {
                            colors.secondary
                        })
                        .child(range_label(days))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.account_usage_days = days;
                            this.account_usage_chart_split = None;
                            cx.notify();
                        }))
                }))
                .child(
                    div()
                        .id("inspector-usage-cost")
                        .px(px(8.0))
                        .py(px(3.0))
                        .rounded(px(6.0))
                        .bg(if !tokens {
                            colors.primary.alpha(0.12)
                        } else {
                            colors.primary.alpha(0.05)
                        })
                        .cursor_pointer()
                        .text_size(px(11.0))
                        .text_color(if !tokens {
                            colors.primary
                        } else {
                            colors.secondary
                        })
                        .child("Cost")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.account_usage_tokens = false;
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .id("inspector-usage-tokens")
                        .px(px(8.0))
                        .py(px(3.0))
                        .rounded(px(6.0))
                        .bg(if tokens {
                            colors.primary.alpha(0.12)
                        } else {
                            colors.primary.alpha(0.05)
                        })
                        .cursor_pointer()
                        .text_size(px(11.0))
                        .text_color(if tokens {
                            colors.primary
                        } else {
                            colors.secondary
                        })
                        .child("Tokens")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.account_usage_tokens = true;
                            cx.notify();
                        })),
                ),
        );

        if !self.account_usage.remote.is_empty() {
            let mut sources = div().flex().flex_wrap().gap(px(4.0)).px(px(2.0));
            sources = sources
                .child(
                    div()
                        .id("inspector-usage-source-all")
                        .px(px(8.0))
                        .py(px(3.0))
                        .rounded(px(6.0))
                        .bg(if self.account_usage_host.is_none() {
                            colors.primary.alpha(0.12)
                        } else {
                            colors.primary.alpha(0.05)
                        })
                        .cursor_pointer()
                        .text_size(px(11.0))
                        .text_color(colors.secondary)
                        .child("All machines")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.account_usage_host = None;
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .id("inspector-usage-source-local")
                        .px(px(8.0))
                        .py(px(3.0))
                        .rounded(px(6.0))
                        .bg(if self.account_usage_host.as_deref() == Some("") {
                            colors.primary.alpha(0.12)
                        } else {
                            colors.primary.alpha(0.05)
                        })
                        .cursor_pointer()
                        .text_size(px(11.0))
                        .text_color(colors.secondary)
                        .child("This Mac")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.account_usage_host = Some(String::new());
                            cx.notify();
                        })),
                );
            for host in &self.account_usage.remote {
                let id = host.host.clone();
                let selected = self.account_usage_host.as_deref() == Some(id.as_str());
                sources = sources.child(
                    div()
                        .id(SharedString::from(format!("inspector-usage-source-{id}")))
                        .px(px(8.0))
                        .py(px(3.0))
                        .rounded(px(6.0))
                        .bg(if selected {
                            colors.primary.alpha(0.12)
                        } else {
                            colors.primary.alpha(0.05)
                        })
                        .cursor_pointer()
                        .text_size(px(11.0))
                        .text_color(colors.secondary)
                        .child(host.name.clone())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.account_usage_host = Some(id.clone());
                            cx.notify();
                        })),
                );
            }
            view = view.child(sources);
            for host in &self.account_usage.remote {
                if self
                    .account_usage_host
                    .as_deref()
                    .is_none_or(|id| id == host.host)
                {
                    let last = host.data.as_ref().map(|data| {
                        let time = data.collected_at;
                        format!(
                            "{} {:02}:{:02} UTC",
                            date_label(time.div_euclid(86_400)),
                            time.rem_euclid(86_400) / 3_600,
                            time.rem_euclid(3_600) / 60
                        )
                    });
                    let status = match (host.status, last) {
                        (RemoteUsageStatus::Loading, Some(last)) => {
                            format!("Updating · cached through {last}")
                        }
                        (RemoteUsageStatus::Loading, None) => "Reading remote usage…".to_owned(),
                        (RemoteUsageStatus::Unavailable, Some(last)) => {
                            format!("Unavailable · showing usage saved at {last}")
                        }
                        (RemoteUsageStatus::Unavailable, None) => {
                            "Usage unavailable · check the connection in Remote settings".to_owned()
                        }
                        (RemoteUsageStatus::Ready, Some(last)) => format!("Updated {last}"),
                        (RemoteUsageStatus::Ready, None) => "No transcript history".to_owned(),
                    };
                    view = view.child(message(format!("{} · {status}", host.name), colors));
                }
            }
        }

        let hero_value = if tokens {
            UsageFormat::tokens(total.total_tokens())
        } else {
            format!("${:.2}", total.cost)
        };
        view = view.child(section_label("Total", colors)).child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .px(px(2.0))
                .child(
                    div()
                        .flex()
                        .items_start()
                        .gap(px(6.0))
                        .child(self.account_usage_numbers.show(
                            "inspector-usage-hero",
                            if tokens {
                                total.total_tokens() as f64
                            } else {
                                total.cost
                            },
                            hero_value,
                            30.0,
                            colors.primary,
                            FontWeight::MEDIUM,
                        ))
                        .when_some(
                            if tokens {
                                compare.processed_tokens_change()
                            } else {
                                compare.cost_change()
                            },
                            |row, change| {
                                row.child(account_change_delta(
                                    &self.account_usage_numbers,
                                    "inspector-usage-hero-delta",
                                    change,
                                    colors,
                                ))
                            },
                        ),
                )
                .child(message(
                    "Claude and Codex at model rates. Cursor is billed usage.",
                    colors,
                )),
        );

        for index in self.account_usage_provider_order() {
            let provider = &report.providers[index];
            let provider_tokens = provider.totals().total_tokens();
            let share = if tokens {
                ratio(provider_tokens as f64, total.total_tokens() as f64)
            } else {
                ratio(provider.tokens.c, total.cost)
            };
            let amount = if tokens {
                self.account_usage_numbers.show(
                    format!("inspector-usage-provider-{index}"),
                    provider_tokens as f64,
                    UsageFormat::tokens(provider_tokens),
                    13.0,
                    colors.primary,
                    FontWeight::NORMAL,
                )
            } else {
                self.account_usage_numbers.show(
                    format!("inspector-usage-provider-{index}"),
                    provider.tokens.c,
                    format!("${:.2}", provider.tokens.c),
                    13.0,
                    colors.primary,
                    FontWeight::NORMAL,
                )
            };
            view = view.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .px(px(2.0))
                    .pt(px(6.0))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .gap(px(12.0))
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(colors.primary)
                                    .child(PROVIDERS[index]),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_start()
                                    .gap(px(6.0))
                                    .child(amount)
                                    .when_some(
                                        if tokens {
                                            compare.provider_tokens_change(index)
                                        } else {
                                            compare.provider_cost_change(index)
                                        },
                                        |row, change| {
                                            row.child(account_change_delta(
                                                &self.account_usage_numbers,
                                                format!("inspector-usage-delta-provider-{index}"),
                                                change,
                                                colors,
                                            ))
                                        },
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .h(px(3.0))
                            .w_full()
                            .rounded(px(2.0))
                            .bg(colors.primary.alpha(0.06))
                            .child(
                                div()
                                    .h_full()
                                    .w(gpui::relative(share as f32))
                                    .rounded(px(2.0))
                                    .bg(provider_color(index, colors)),
                            ),
                    )
                    .child(message(
                        if tokens {
                            format!(
                                "{:.1}% of tokens · ${:.2}",
                                share * 100.0,
                                provider.tokens.c
                            )
                        } else {
                            format!(
                                "{:.1}% of cost · {} tokens",
                                share * 100.0,
                                UsageFormat::tokens(provider_tokens)
                            )
                        },
                        colors,
                    )),
            );
        }

        view = view.child(self.account_usage_chart(&chart_providers, colors, cx));

        view = view.child(section_label("Details", colors));
        let input = total.input_tokens + total.cache_read_tokens + total.cache_write_tokens;
        for (id, title, value, change, caption) in [
            (
                "processed",
                "Processed tokens",
                UsageFormat::tokens(total.total_tokens()),
                compare.processed_tokens_change(),
                format!(
                    "{} per active day",
                    UsageFormat::tokens(total.total_tokens() / report.active_days.max(1) as i64)
                ),
            ),
            (
                "cached",
                "Cached input",
                UsageFormat::tokens(total.cache_read_tokens),
                compare.cached_input_change(),
                format!(
                    "{:.1}% of input",
                    ratio(total.cache_read_tokens as f64, input as f64) * 100.0
                ),
            ),
            (
                "uncached",
                "Uncached input",
                UsageFormat::tokens(total.input_tokens),
                compare.uncached_input_change(),
                format!(
                    "{} cache writes",
                    UsageFormat::tokens(total.cache_write_tokens)
                ),
            ),
            (
                "output",
                "Output",
                UsageFormat::tokens(total.output_tokens),
                compare.output_change(),
                format!(
                    "{} reasoning reported",
                    UsageFormat::tokens(report.total.reasoning)
                ),
            ),
            (
                "savings",
                "Cache read savings",
                format!("${:.2}", report.total.read_savings),
                compare.read_savings_change(),
                "Estimated · excludes writes".to_owned(),
            ),
        ] {
            view = view.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .px(px(2.0))
                    .pt(px(6.0))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .gap(px(12.0))
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(colors.secondary)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_start()
                                    .gap(px(6.0))
                                    .child(
                                        div()
                                            .text_size(px(13.0))
                                            .text_color(colors.primary)
                                            .child(value),
                                    )
                                    .when_some(change, |row, change| {
                                        row.child(account_change_delta(
                                            &self.account_usage_numbers,
                                            format!("inspector-usage-delta-metric-{id}"),
                                            change,
                                            colors,
                                        ))
                                    }),
                            ),
                    )
                    .child(message(caption, colors)),
            );
        }

        view = view.child(section_label("Breakdown", colors));
        for row in &report.models {
            let detail_total = row.detail.totals();
            let cost = if row.detail.priced_tokens == 0 {
                "Unpriced".to_owned()
            } else {
                format!("${:.2}", detail_total.cost)
            };
            view = view.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .px(px(2.0))
                    .pt(px(4.0))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(colors.primary)
                            .child(format!("{} · {}", PROVIDERS[row.provider], row.model)),
                    )
                    .child(message(
                        format!(
                            "{cost} · {:.1}% · {} tokens",
                            ratio(detail_total.cost, report.total.tokens.c) * 100.0,
                            UsageFormat::tokens(detail_total.total_tokens())
                        ),
                        colors,
                    )),
            );
        }

        view = view
            .child(section_label("About these estimates", colors))
            .child(message(
                format!(
                    "{:.1}% of tokens priced · {} unpriced tokens",
                    ratio(
                        report.total.priced_tokens as f64,
                        total.total_tokens() as f64
                    ) * 100.0,
                    UsageFormat::tokens(total.total_tokens() - report.total.priced_tokens)
                ),
                colors,
            ))
            .child(message(
                "Uses Ubra's bundled model rates for Claude and Codex. Cursor costs come from billed dashboard events. Unpriced Claude/Codex usage is excluded from cost.",
                colors,
            ));
        view.into_any_element()
    }

    fn account_usage_chart(
        &self,
        providers: &[Vec<ChartSample>; 3],
        colors: SemanticColors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tokens = self.account_usage_tokens;
        let window_days = self.account_usage_days as f32;
        let end = providers[0].last().map(|sample| sample.time).unwrap_or(0);
        let series = self.account_chart_series(providers, window_days, end);
        let line_colors: Vec<Rgba> = match self.account_usage_chart_split {
            None => vec![colors.primary],
            Some(visible) => self
                .account_usage_provider_order()
                .into_iter()
                .filter(|&index| visible[index])
                .map(|index| provider_color(index, colors))
                .collect(),
        };
        let (_, range_max) = usage_chart::series_range(&series);
        let y_max = if range_max > 0.0 { range_max } else { 1.0 };
        let first_time = series
            .first()
            .and_then(|line| line.first())
            .map(|sample| sample.time)
            .unwrap_or(end);
        let last_time = series
            .first()
            .and_then(|line| line.last())
            .map(|sample| sample.time)
            .unwrap_or(end);
        let axis = |hour: i64| {
            if window_days < 2.0 {
                hour_label(hour)
            } else {
                date_label(hour.div_euclid(24))
            }
        };
        let paint_lines: Vec<(Vec<ChartSample>, Rgba)> =
            series.into_iter().zip(line_colors).collect();
        let bounds_slot = std::rc::Rc::new(std::cell::Cell::new(None::<Bounds<Pixels>>));
        let split = self.account_usage_chart_split;
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .px(px(2.0))
            .pt(px(8.0))
            .child(section_label("Trend", colors))
            .child(
                div()
                    .flex()
                    .gap(px(4.0))
                    .child(
                        div()
                            .id("inspector-usage-series-all")
                            .px(px(8.0))
                            .py(px(3.0))
                            .rounded(px(6.0))
                            .bg(if split.is_none() {
                                colors.primary.alpha(0.12)
                            } else {
                                colors.primary.alpha(0.05)
                            })
                            .cursor_pointer()
                            .text_size(px(11.0))
                            .text_color(colors.secondary)
                            .child("All")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.account_usage_chart_split = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("inspector-usage-series-individual")
                            .px(px(8.0))
                            .py(px(3.0))
                            .rounded(px(6.0))
                            .bg(if split.is_some() {
                                colors.primary.alpha(0.12)
                            } else {
                                colors.primary.alpha(0.05)
                            })
                            .cursor_pointer()
                            .text_size(px(11.0))
                            .text_color(colors.secondary)
                            .child("Individual")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.account_usage_chart_split = Some([true; 3]);
                                cx.notify();
                            })),
                    )
                    .when(split.is_some(), |row| {
                        row.child(
                            div().text_size(px(11.0)).text_color(colors.tertiary).child(
                                self.account_usage_provider_order()
                                    .into_iter()
                                    .map(|index| SERIES_LABELS[index])
                                    .collect::<Vec<_>>()
                                    .join(" · "),
                            ),
                        )
                    }),
            )
            .child(
                div().relative().w_full().h(px(140.0)).child(
                    canvas(
                        {
                            let bounds_slot = std::rc::Rc::clone(&bounds_slot);
                            move |bounds, _, _| bounds_slot.set(Some(bounds))
                        },
                        move |bounds, _, window, _| {
                            usage_chart::paint(
                                window,
                                bounds,
                                &paint_lines,
                                window_days,
                                0.0,
                                y_max,
                                end,
                                None,
                            );
                        },
                    )
                    .absolute()
                    .inset_0(),
                ),
            )
            .child(
                div()
                    .flex()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(colors.tertiary)
                            .child(axis(first_time)),
                    )
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(colors.tertiary)
                            .child(axis(last_time)),
                    ),
            )
            .when(tokens, |chart| {
                chart.child(message(
                    "Shown in tokens; switch to Cost for spend.",
                    colors,
                ))
            })
            .into_any_element()
    }
}

fn range_label(days: usize) -> SharedString {
    match days {
        1 => "24h".into(),
        7 => "7d".into(),
        30 => "1M".into(),
        90 => "3M".into(),
        days => format!("{days}d").into(),
    }
}

fn ratio(value: f64, total: f64) -> f64 {
    if total > 0.0 {
        (value / total).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn provider_color(provider: usize, colors: SemanticColors) -> Rgba {
    match provider {
        0 => rgba(0xcf876dff),
        2 => rgba(0x6d8fcfff),
        _ => colors.secondary,
    }
}

fn account_change_delta(
    numbers: &crate::number_flow::Bank,
    id: impl Into<String>,
    change: f64,
    colors: SemanticColors,
) -> gpui::Div {
    let pct = (change * 100.0).round() as i64;
    if pct == 0 {
        return div()
            .text_size(px(11.0))
            .text_color(colors.secondary)
            .child("Same");
    }
    let (mark, amount, color) = if pct > 0 {
        ("▲", pct, Ink::FRESH)
    } else {
        ("▼", -pct, Ink::DANGER)
    };
    div()
        .flex()
        .items_start()
        .gap(px(3.0))
        .child(div().text_size(px(9.0)).text_color(color).child(mark))
        .child(numbers.show(
            id,
            amount as f64,
            format!("{amount}%"),
            11.0,
            color,
            FontWeight::NORMAL,
        ))
}
