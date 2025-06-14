#! /usr/bin/env Rscript --vanilla

library(tidyverse)
library(patchwork)
library(cowplot)
library(scales)

simd.hps <- 4e6 / 0.044254
browser.hps <- 5e6 / mean(c(
        0.105, 1.69, 1.06, 1.89, 1.91, 1.09, 1.80, 0.97, 0.71, 1.15, 3.59, 1.09, 0.14, 3.98, 1.26, 1.05, 1.26
    ))

browser.cacm.hps <- 14760000 / 13.7065

hps <- tribble(
    ~environment, ~hps,
    "AVX-512 Adversarially Optimized (32 threads E2E)", 5e6 * 13761 / 60,
    "AVX-512 Adversarially Optimized (1 thread)", simd.hps,
    "SHA-NI Safely Optimized", 4e6 / 0.092932,
    "autovectorized official build (1 thread)", 4e6 / 0.22714,
    "generic official build (1 thread)", 4e6 / 0.48967,
    "official WASM build", browser.hps,
) |>
    mutate(
        environment = fct_reorder(environment, hps, .desc = TRUE),
    )

plots <- list()

p <- map2(c(1000000, 5000000, 10000000, 50000000), c("bottom", "none", "bottom", "none"), \(difficulty, legend.position) {
    df <- tibble(iterations = 2^seq(0, 40, by=0.01)) |>
        mutate(p = pgeom(iterations, 1/difficulty))
    
    simd.t50 <- qgeom(0.5, 1/difficulty) / simd.hps
    simd.t99 <- qgeom(0.99, 1/difficulty) / simd.hps
    browser.t50 <- qgeom(0.5, 1/difficulty) / browser.hps
    browser.t99 <- qgeom(0.99, 1/difficulty) / browser.hps
    official.cacm.t50 <- qgeom(0.5, 1/difficulty) / browser.cacm.hps
    official.cacm.t95 <- qgeom(0.95, 1/difficulty) / browser.cacm.hps
    official.cacm.t99 <- qgeom(0.99, 1/difficulty) / browser.cacm.hps

    df |> 
        cross_join(hps) |>
        mutate(time = iterations / hps) |>
        ggplot(aes(x = time, y = p, color = environment)) +
        geom_line() +
        labs(
            title = sprintf("Time to solve for difficulty %d (extrapolated throughput)", difficulty),
            x = "Time to solve",
            y = "P (X <= t)",
            caption = sprintf("SIMD: (%.2fs 50%%, %.2fs 99%%), browser: (%.2fs 50%%, %.2fs 99%%), CACM User Survey: (%.2fs 50%%, %.2fs 99%%)", simd.t50, simd.t99, browser.t50, browser.t99, official.cacm.t50, official.cacm.t99)
        ) +
        scale_x_continuous(
          limits = c(0, browser.t99),
          labels = label_timespan(unit = "secs"),
          breaks = seq(0, 1, by = 0.1) * max(browser.t99, official.cacm.t95)
        ) +
        geom_hline(yintercept = 0.95, linetype = "dashed", color = "navy") +
        annotate(
            "text",
            x = max(browser.t99, official.cacm.t95),
            y = 0.90,
            label = "95% challenges complete",
            hjust = 1,
            vjust = 1,
            color = "black"
        ) +
        theme_cowplot() +
        theme(legend.position = legend.position)
}) |> reduce(`+`)

ggsave("time.png", p, width = 18, height = 12, bg = "white", dpi = 300)
