import { expect, test, type Page } from "@playwright/test";
import type { VoiceSnapshot } from "../src/overlay/VoiceOverlay";

declare global {
  interface Window {
    __voiceEmit: (snapshot: VoiceSnapshot) => void;
    __voiceLevels: (levels: unknown) => void;
    __voiceListenerCount: (event: string) => number;
    __voiceCalls: { command: string; args: Record<string, unknown> }[];
    __voiceDelayCopy: boolean;
    __voiceFinishCopy: () => void;
  }
}

const initial: VoiceSnapshot = {
  generation: 4,
  phrase: 1,
  revision: 1,
  status: "loading",
  committed: "",
  tentative: "",
  draft: "",
  error: null,
  speech_active: false,
  silence_remaining_ms: null,
  send_remaining_ms: null,
};

async function setup(page: Page, language = "ru") {
  await page.addInitScript(
    ({ snapshot, language }) => {
      const callbacks = new Map<number, (payload: unknown) => void>();
      const listeners = new Map<number, { event: string; handler: number }>();
      let id = 0;
      window.__voiceCalls = [];
      window.__voiceDelayCopy = false;
      Object.defineProperty(window, "__TAURI_INTERNALS__", {
        value: {
          metadata: {
            currentWindow: { label: "recording_overlay" },
            currentWebview: { label: "recording_overlay" },
          },
          transformCallback: (callback: (payload: unknown) => void) => {
            callbacks.set(++id, callback);
            return id;
          },
          unregisterCallback: (key: number) => callbacks.delete(key),
          invoke: async (
            command: string,
            args: Record<string, unknown> = {},
          ) => {
            window.__voiceCalls.push({ command, args });
            if (command === "plugin:event|listen") {
              listeners.set(++id, {
                event: String(args.event),
                handler: Number(args.handler),
              });
              return id;
            }
            if (command === "plugin:event|unlisten") {
              listeners.delete(Number(args.eventId));
              return null;
            }
            if (command === "voice_session_snapshot") return snapshot;
            if (command === "voice_copy_draft" && window.__voiceDelayCopy) {
              return new Promise<void>((resolve) => {
                window.__voiceFinishCopy = resolve;
              });
            }
            if (command === "get_app_settings")
              return {
                app_language: language,
                theme: "dark",
                overlay_position: "bottom",
              };
            return null;
          },
        },
      });
      Object.defineProperty(window, "__TAURI_EVENT_PLUGIN_INTERNALS__", {
        value: {
          unregisterListener: (_event: string, key: number) =>
            listeners.delete(key),
        },
      });
      const emitEvent = (event: string, payload: unknown) => {
        for (const [key, listener] of listeners) {
          if (listener.event === event)
            callbacks.get(listener.handler)?.({
              event: listener.event,
              id: key,
              payload,
            });
        }
      };
      window.__voiceEmit = (payload) =>
        emitEvent("voice-session-state", payload);
      window.__voiceLevels = (payload) => emitEvent("mic-level", payload);
      window.__voiceListenerCount = (event) =>
        [...listeners.values()].filter((listener) => listener.event === event)
          .length;
      localStorage.setItem("theme", "dark");
    },
    { snapshot: initial, language },
  );
  await page.goto("/src/overlay/index.html");
  await expect(page.locator(".voice-panel")).toHaveAttribute(
    "data-status",
    "loading",
  );
}

async function emit(page: Page, change: Partial<VoiceSnapshot>) {
  await page.evaluate((snapshot) => window.__voiceEmit(snapshot), {
    ...initial,
    ...change,
  });
}

for (const language of ["en", "ru"]) {
  test(`send hint fits and uncertain delivery keeps explicit recovery in ${language}`, async ({
    page,
  }) => {
    await setup(page, language);
    await emit(page, { revision: 2, status: "listening" });
    await expect(page.locator(".voice-status")).toHaveText(
      language === "ru" ? "Слушаю" : "Listening",
    );
    await expect(page.locator(".voice-status")).not.toContainText(
      /Enter|\d+\s*[sс]/,
    );
    const hint = page.locator(".voice-hint");
    await expect(hint).toBeVisible();
    await expect(hint).toHaveText(
      language === "ru"
        ? "Пауза 1 с — вставка и отправка"
        : "Pause 1s pastes and sends",
    );
    const box = await page.locator(".voice-panel").boundingBox();
    expect(box?.height).toBe(42);
    expect(box?.width).toBe(380);
    expect(
      await page
        .locator(".voice-status")
        .evaluate(
          (el) => el.scrollWidth <= el.clientWidth && el.scrollHeight <= 20,
        ),
    ).toBe(true);
    await page.screenshot({ path: `/tmp/diktor-send-hint-${language}.png` });
    await emit(page, {
      revision: 3,
      status: "committing",
      committed: "Диктор отправляй дословно",
    });
    await expect(page.locator(".voice-status")).toHaveText(
      language === "ru" ? "Отправляю…" : "Sending…",
    );
    await expect(hint).toBeHidden();
    await emit(page, {
      revision: 4,
      status: "error",
      error: "delivery_uncertain",
      draft: "Диктор отправляй дословно",
    });
    await expect(page.locator(".voice-status")).toHaveText(
      language === "ru"
        ? "Текст мог быть вставлен или отправлен. Проверьте окно; черновик сохранён."
        : "Text may already be inserted or sent. Check the window; your draft is saved.",
    );
    await expect(page.getByTestId("voice-text")).toHaveText(
      "Диктор отправляй дословно",
    );
    const copy = page.getByRole("button", {
      name: language === "ru" ? "Скопировать черновик" : "Copy draft",
    });
    await expect(copy).toBeVisible();
    expect(
      await page
        .locator(".voice-panel")
        .evaluate((el) => el.scrollHeight <= el.clientHeight),
    ).toBe(true);
    expect(
      await page.evaluate(() =>
        window.__voiceCalls.some((call) => call.command === "voice_copy_draft"),
      ),
    ).toBe(false);
    await page.screenshot({
      path: `/tmp/diktor-send-uncertain-${language}.png`,
    });
    await copy.click();
    await expect(page.locator(".voice-recovery")).toContainText(
      language === "ru" ? "Скопировано" : "Copied",
    );
  });

  test(`model error classifications retain draft and recovery in ${language}`, async ({
    page,
  }) => {
    await setup(page, language);
    const messages =
      language === "ru"
        ? {
            out_of_memory:
              "Недостаточно памяти для распознавания. Попробуйте модель поменьше.",
            unsupported_model:
              "Модель не поддерживает выбранное распознавание в реальном времени.",
            model:
              "Распознавание прервалось. Выключите режим и попробуйте снова.",
          }
        : {
            out_of_memory:
              "Not enough memory for recognition. Try a smaller model.",
            unsupported_model:
              "This model does not support the requested live recognition.",
            model:
              "Recognition stopped unexpectedly. Turn voice mode off and try again.",
          };
    let revision = 2;
    await emit(page, {
      revision: revision++,
      status: "error",
      error: "out_of_memory",
    });
    await expect(page.locator(".voice-status")).toHaveText(
      messages.out_of_memory,
    );
    await expect(page.getByTestId("voice-text")).toHaveCount(0);
    await expect(
      page.getByRole("button", {
        name: language === "ru" ? "Скопировать черновик" : "Copy draft",
      }),
    ).toHaveCount(0);
    await page.screenshot({
      path: `/tmp/diktor-oom-overlay-startup-${language}.png`,
    });
    for (const error of [
      "out_of_memory",
      "unsupported_model",
      "model",
      "unknown_native_failure",
    ] as const) {
      await emit(page, {
        revision: revision++,
        status: "error",
        error,
        draft: "Сохранённый черновик",
      });
      await expect(page.locator(".voice-status")).toContainText(
        messages[error === "unknown_native_failure" ? "model" : error],
      );
      if (error !== "unsupported_model") {
        await expect(page.locator(".voice-status")).not.toContainText(
          messages.unsupported_model,
        );
      }
      await expect(page.getByTestId("voice-text")).toHaveText(
        "Сохранённый черновик",
      );
      await expect(
        page.getByRole("button", {
          name: language === "ru" ? "Скопировать черновик" : "Copy draft",
        }),
      ).toBeVisible();
      const box = await page.locator(".voice-panel").boundingBox();
      expect(box?.height).toBeLessThanOrEqual(140);
      if (error === "out_of_memory")
        await page.screenshot({
          path: `/tmp/diktor-oom-overlay-${language}.png`,
        });
    }
    expect(
      await page.evaluate(() =>
        window.__voiceCalls.some((call) => call.command === "voice_copy_draft"),
      ),
    ).toBe(false);
  });
}

test("loading, immediate speech activity and exact live whitespace", async ({
  page,
}) => {
  await setup(page);
  await expect(page.locator(".voice-panel")).toHaveText("Подключаю…");
  await emit(page, { revision: 2, status: "listening", speech_active: true });
  await expect(page.locator(".voice-panel")).toHaveText("Слышу речь…");
  await emit(page, {
    revision: 3,
    status: "listening",
    committed: "Первая  часть ",
    tentative: "и продолжение",
  });
  expect(await page.getByTestId("voice-text").textContent()).toBe(
    "Первая  часть и продолжение",
  );
  expect(
    await page
      .locator(".voice-committed")
      .evaluate((el) => getComputedStyle(el).fontSize),
  ).toBe("14px");
  const box = await page.locator(".voice-panel").boundingBox();
  expect(box?.width).toBe(380);
  expect(box?.height).toBe(42);
  await page.screenshot({ path: "/tmp/diktor-voice-listening.png" });
});

test("stale generation, phrase and revision cannot revive text or a stopped mode", async ({
  page,
}) => {
  await setup(page);
  await emit(page, {
    revision: 10,
    phrase: 2,
    status: "listening",
    committed: "Новая фраза",
  });
  for (const stale of [
    { generation: 3, revision: 100, phrase: 3 },
    { phrase: 1, revision: 11 },
    { phrase: 2, revision: 9 },
  ]) {
    await emit(page, { status: "listening", committed: "Старое", ...stale });
  }
  await expect(page.getByTestId("voice-text")).toHaveText("Новая фраза");
  await emit(page, { revision: 11, phrase: 2, status: "off" });
  await emit(page, {
    revision: 10,
    phrase: 2,
    status: "listening",
    committed: "Поздний ответ",
  });
  await expect(page.locator(".voice-panel")).toBeHidden();
});

test("long draft stays compact, scrolls to latest and only explicit Copy invokes clipboard", async ({
  page,
}) => {
  await setup(page);
  const draft = "Длинная фраза с сохранёнными пробелами. ".repeat(25);
  await emit(page, {
    revision: 2,
    status: "draft",
    error: "target_changed",
    draft,
  });
  await expect(page.getByTestId("voice-text")).toHaveText(draft);
  const box = await page.locator(".voice-panel").boundingBox();
  expect(box?.height).toBeLessThanOrEqual(140);
  expect(
    await page.getByTestId("voice-text").evaluate((el) => el.scrollTop > 0),
  ).toBe(true);
  expect(
    await page.evaluate(() =>
      window.__voiceCalls.some((call) => call.command === "voice_copy_draft"),
    ),
  ).toBe(false);
  await page.screenshot({ path: "/tmp/diktor-voice-draft.png" });
  await page.getByRole("button", { name: "Скопировать черновик" }).click();
  await expect(page.locator(".voice-recovery")).toContainText("Скопировано");
  const copies = await page.evaluate(() =>
    window.__voiceCalls.filter((call) => call.command === "voice_copy_draft"),
  );
  expect(copies).toEqual([
    { command: "voice_copy_draft", args: { generation: 4, phrase: 1 } },
  ]);
});

test("commit and fault preserve readable draft and report bounded native height", async ({
  page,
}) => {
  await setup(page);
  await emit(page, {
    revision: 2,
    status: "committing",
    committed: "Фраза перед вставкой",
  });
  await expect(page.locator(".voice-status")).toHaveText("Отправляю…");
  await emit(page, {
    revision: 3,
    status: "error",
    error: "backlog",
    draft: "Сохранённый текст",
  });
  await expect(page.getByTestId("voice-text")).toHaveText("Сохранённый текст");
  await expect(page.locator(".voice-status")).toContainText("Режим остановлен");
  await expect
    .poll(
      async () =>
        (
          await page.evaluate(() =>
            window.__voiceCalls.filter(
              (call) => call.command === "voice_overlay_height",
            ),
          )
        ).length,
    )
    .toBeGreaterThan(0);
  const heights = await page.evaluate(() =>
    window.__voiceCalls
      .filter((call) => call.command === "voice_overlay_height")
      .map((call) => Number(call.args.height)),
  );
  expect(heights.every((height) => height >= 42 && height <= 140)).toBe(true);
});

test("late Copy completion cannot label a new generation copied", async ({
  page,
}) => {
  await setup(page);
  await page.evaluate(() => {
    window.__voiceDelayCopy = true;
  });
  await emit(page, {
    revision: 2,
    status: "draft",
    error: "target_changed",
    draft: "Старый черновик",
  });
  await page.getByRole("button", { name: "Скопировать черновик" }).click();
  await emit(page, {
    generation: 5,
    revision: 1,
    status: "draft",
    error: "target_changed",
    draft: "Новый черновик",
  });
  await expect(page.getByTestId("voice-text")).toHaveText("Новый черновик");
  await page.evaluate(() => window.__voiceFinishCopy());
  await expect(page.locator(".voice-recovery")).not.toContainText(
    "Скопировано",
  );
});

async function barHeights(page: Page) {
  return page
    .locator(".voice-levels > span")
    .evaluateAll((bars) =>
      bars.map((bar) => Number.parseFloat((bar as HTMLElement).style.height)),
    );
}

test("five bars follow real buckets, clamp invalid values and expire without events", async ({
  page,
}) => {
  await setup(page);
  await page.clock.install();
  await emit(page, { revision: 2, status: "listening" });
  await expect(page.locator(".voice-levels")).toHaveAttribute(
    "aria-hidden",
    "true",
  );
  expect(await barHeights(page)).toEqual([2, 2, 2, 2, 2]);
  for (const [level, height] of [
    [0.1, 3.2],
    [1, 14],
    [0, 2],
  ]) {
    await page.evaluate(
      (level) => window.__voiceLevels(Array(16).fill(level)),
      level,
    );
    await page.clock.runFor(34);
    expect(await barHeights(page)).toEqual(Array(5).fill(height));
  }
  await page.evaluate(() =>
    window.__voiceLevels([
      0, 0, 0, 0.25, 0.25, 0.25, 0.5, 0.5, 0.5, 0.75, 0.75, 0.75, 1, 1, 1, 1,
    ]),
  );
  await page.clock.runFor(34);
  expect(await barHeights(page)).toEqual([2, 5, 8, 11, 14]);
  await page.evaluate(() =>
    window.__voiceLevels([
      NaN,
      Infinity,
      -Infinity,
      -1,
      -2,
      -3,
      2,
      3,
      4,
      null,
      "1",
      undefined,
      0.5,
      0.5,
      0.5,
      0.5,
    ]),
  );
  await page.clock.runFor(34);
  expect(await barHeights(page)).toEqual([2, 2, 14, 2, 8]);
  await page.evaluate(() => window.__voiceLevels({}));
  await page.clock.runFor(34);
  expect(await barHeights(page)).toEqual([2, 2, 2, 2, 2]);
  await page.evaluate(() => window.__voiceLevels(Array(16).fill(1)));
  await page.clock.runFor(34);
  await page.clock.runFor(200);
  expect(await barHeights(page)).toEqual([14, 14, 14, 14, 14]);
  await page.clock.runFor(20);
  expect(await barHeights(page)).toEqual([2, 2, 2, 2, 2]);
});

test("level redraw is bounded and generation/status changes clean pending telemetry", async ({
  page,
}) => {
  await setup(page);
  const baseListeners = await page.evaluate(() =>
    window.__voiceListenerCount("mic-level"),
  );
  await page.clock.install();
  await emit(page, { revision: 2, status: "listening" });
  await expect
    .poll(() => page.evaluate(() => window.__voiceListenerCount("mic-level")))
    .toBe(baseListeners + 1);
  await page.evaluate(() => window.__voiceLevels(Array(16).fill(0.5)));
  expect(await barHeights(page)).toEqual([8, 8, 8, 8, 8]);
  for (let i = 0; i < 10; i++) {
    await page.evaluate(() => window.__voiceLevels(Array(16).fill(1)));
    await page.clock.runFor(1);
  }
  expect(await barHeights(page)).toEqual([8, 8, 8, 8, 8]);
  await page.clock.runFor(24);
  expect(await barHeights(page)).toEqual([14, 14, 14, 14, 14]);
  await page.evaluate(() => window.__voiceLevels(Array(16).fill(0.5)));
  await emit(page, { generation: 5, revision: 1, status: "listening" });
  await page.clock.runFor(40);
  expect(await barHeights(page)).toEqual([2, 2, 2, 2, 2]);
  await expect
    .poll(() => page.evaluate(() => window.__voiceListenerCount("mic-level")))
    .toBe(baseListeners + 1);
  for (const [i, status] of (
    ["committing", "draft", "error", "off"] as const
  ).entries()) {
    await page.evaluate(() => {
      window.__voiceLevels(Array(16).fill(1));
      window.__voiceLevels(Array(16).fill(0.5));
    });
    await emit(page, { generation: 5, revision: 2 + i * 2, status });
    await expect(page.locator(".voice-levels")).toHaveCount(0);
    await expect
      .poll(() => page.evaluate(() => window.__voiceListenerCount("mic-level")))
      .toBe(baseListeners);
    await emit(page, {
      generation: 5,
      revision: 3 + i * 2,
      status: "listening",
    });
    await page.clock.runFor(300);
    expect(await barHeights(page)).toEqual([2, 2, 2, 2, 2]);
  }
});

test("silence strip follows snapshot revisions only and telemetry preserves text scroll", async ({
  page,
}) => {
  await setup(page);
  await page.emulateMedia({ reducedMotion: "reduce" });
  await page.clock.install();
  const committed = "Длинная фраза с сохранёнными пробелами. ".repeat(25);
  await emit(page, { revision: 2, status: "listening", committed });
  const text = page.getByTestId("voice-text");
  await text.evaluate((el) => {
    el.scrollTop = 0;
  });
  const strip = page.locator(".voice-silence");
  for (const [revision, remaining] of [
    [3, 500],
    [4, 1],
    [5, 0],
  ]) {
    await emit(page, {
      revision,
      status: "listening",
      committed,
      silence_remaining_ms: remaining,
    });
    await expect(strip).toHaveAttribute("aria-hidden", "true");
    expect(
      await strip.evaluate((el) => (el as HTMLElement).style.transform),
    ).toBe(`scaleX(${remaining / 1000})`);
    expect(await strip.evaluate((el) => getComputedStyle(el).height)).toBe(
      "2px",
    );
    await page.clock.runFor(1500);
    expect(
      await strip.evaluate((el) => (el as HTMLElement).style.transform),
    ).toBe(`scaleX(${remaining / 1000})`);
    expect(await text.evaluate((el) => el.scrollTop)).toBe(0);
    await expect(text).toHaveText(committed);
  }
  await emit(page, {
    revision: 4,
    status: "listening",
    committed,
    silence_remaining_ms: 1000,
  });
  expect(
    await strip.evaluate((el) => (el as HTMLElement).style.transform),
  ).toBe("scaleX(0)");
  await emit(page, {
    revision: 6,
    status: "listening",
    committed,
    silence_remaining_ms: null,
  });
  await expect(strip).toHaveCount(0);
  await emit(page, {
    revision: 5,
    status: "listening",
    committed,
    silence_remaining_ms: 500,
  });
  await expect(strip).toHaveCount(0);
  await emit(page, {
    revision: 7,
    status: "listening",
    committed,
    speech_active: true,
    silence_remaining_ms: 500,
  });
  await expect(strip).toHaveCount(0);
  await emit(page, {
    revision: 8,
    status: "committing",
    committed,
    silence_remaining_ms: 500,
  });
  await expect(strip).toHaveCount(0);
  await emit(page, { generation: 5, revision: 1, status: "listening" });
  await expect(strip).toHaveCount(0);
  expect(await page.locator(".voice-panel").boundingBox()).toMatchObject({
    width: 380,
    height: 42,
  });
});

test("send strip counts 3 s to Enter after paste and hides on speech or phrase", async ({
  page,
}) => {
  await setup(page, "en");
  await emit(page, { revision: 2, status: "listening" });
  const strip = page.locator(".voice-silence");
  await expect(strip).toHaveCount(0);
  await emit(page, {
    revision: 3,
    status: "listening",
    send_remaining_ms: 1500,
  });
  await expect(strip).toHaveAttribute("data-kind", "send");
  expect(
    await strip.evaluate((el) => (el as HTMLElement).style.transform),
  ).toBe("scaleX(0.5)");
  expect(
    await strip.evaluate((el) => getComputedStyle(el).backgroundColor),
  ).not.toBe(
    await page.evaluate(() => {
      const probe = document.createElement("span");
      probe.className = "voice-silence";
      document.querySelector(".voice-panel")?.appendChild(probe);
      const color = getComputedStyle(probe).backgroundColor;
      probe.remove();
      return color;
    }),
  );
  await expect(page.locator(".voice-hint")).toHaveText(
    "Pause 1s pastes and sends",
  );
  await emit(page, {
    revision: 4,
    status: "listening",
    speech_active: true,
    send_remaining_ms: 1500,
  });
  await expect(strip).toHaveCount(0);
  await emit(page, {
    revision: 5,
    status: "listening",
    committed: "next phrase",
    silence_remaining_ms: 250,
    send_remaining_ms: null,
  });
  await expect(strip).toHaveAttribute("data-kind", "paste");
  expect(
    await strip.evaluate((el) => (el as HTMLElement).style.transform),
  ).toBe("scaleX(0.25)");
  await emit(page, {
    revision: 6,
    status: "committing",
    committed: "next phrase",
    send_remaining_ms: 3000,
  });
  await expect(strip).toHaveCount(0);
  await emit(page, { revision: 7, status: "listening", send_remaining_ms: 0 });
  expect(
    await strip.evaluate((el) => (el as HTMLElement).style.transform),
  ).toBe("scaleX(0)");
});

for (const theme of ["light", "dark"]) {
  test(`synthetic waveform and captured silence screenshots on ${theme}`, async ({
    page,
  }) => {
    await setup(page);
    await page.setViewportSize({ width: 460, height: 180 });
    await page.evaluate((theme) => {
      document.documentElement.dataset.theme = theme;
      document.body.style.background =
        theme === "light" ? "#edf0f2" : "#191b1f";
    }, theme);
    await page.clock.install();
    await emit(page, { revision: 2, status: "listening" });
    await page.screenshot({ path: `/tmp/diktor-wave-${theme}-listening.png` });
    await emit(page, { revision: 3, status: "listening", speech_active: true });
    await page.evaluate(() =>
      window.__voiceLevels([
        0.1, 0.2, 0.3, 0.5, 0.6, 0.4, 0.8, 0.9, 0.7, 0.6, 0.5, 0.7, 0.2, 0.3,
        0.4, 0.3,
      ]),
    );
    await page.clock.runFor(34);
    await page.screenshot({ path: `/tmp/diktor-wave-${theme}-speaking.png` });
    await emit(page, {
      revision: 4,
      status: "listening",
      committed: "Мини-волна отражает реальный звук",
      silence_remaining_ms: 500,
    });
    await page.evaluate(() => window.__voiceLevels(Array(16).fill(0)));
    await page.clock.runFor(34);
    await page.screenshot({
      path: `/tmp/diktor-wave-${theme}-half-silence.png`,
    });
    await emit(page, {
      revision: 5,
      status: "draft",
      error: "target_changed",
      draft: "Мини-волна отражает реальный звук",
    });
    await expect(page.locator(".voice-levels, .voice-silence")).toHaveCount(0);
    await page.screenshot({ path: `/tmp/diktor-wave-${theme}-draft.png` });
  });
}
