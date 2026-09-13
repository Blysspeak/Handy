import { expect, test, type Page } from "@playwright/test";
import type { VoiceSnapshot } from "../src/overlay/VoiceOverlay";

declare global {
  interface Window {
    __voiceEmit: (snapshot: VoiceSnapshot) => void;
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
      window.__voiceEmit = (payload) => {
        for (const [key, listener] of listeners) {
          if (listener.event === "voice-session-state")
            callbacks.get(listener.handler)?.({
              event: listener.event,
              id: key,
              payload,
            });
        }
      };
      localStorage.setItem("theme", "dark");
    },
    { snapshot: initial, language },
  );
  await page.goto("/src/overlay/index.html");
  await expect(page.locator(".voice-drop")).toHaveAttribute(
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

/** Every window size the overlay asked the backend for, in order. */
async function sizes(page: Page) {
  return page.evaluate(() =>
    window.__voiceCalls
      .filter((call) => call.command === "voice_overlay_size")
      .map((call) => ({ ...call.args })),
  );
}

async function lastSize(page: Page) {
  return (await sizes(page)).at(-1);
}

const DROPLET_SIZE = { generation: 4, width: 24, height: 32 };

for (const language of ["en", "ru"]) {
  test(`dictation shows only the droplet and uncertain delivery keeps explicit recovery in ${language}`, async ({
    page,
  }) => {
    await setup(page, language);
    const drop = page.locator(".voice-drop");
    const bubble = page.locator(".voice-bubble");
    await emit(page, { revision: 2, status: "listening" });
    await expect(drop).toHaveAttribute("data-tone", "neutral");
    await expect(drop).toHaveAttribute(
      "aria-label",
      language === "ru" ? "Слушаю" : "Listening",
    );
    await emit(page, {
      revision: 3,
      status: "listening",
      speech_active: true,
      committed: "Диктор отправляй ",
      tentative: "дословно",
    });
    await expect(drop).toHaveAttribute("data-tone", "speech");
    await expect(bubble).toHaveCount(0);
    await expect(page.locator(".voice-stage")).toHaveText("");
    await expect(page.locator(".voice-levels, .voice-hint")).toHaveCount(0);
    await emit(page, {
      revision: 4,
      status: "committing",
      committed: "Диктор отправляй дословно",
    });
    await expect(drop).toHaveAttribute("data-tone", "sending");
    await expect(drop).toHaveAttribute(
      "aria-label",
      language === "ru" ? "Отправляю…" : "Sending…",
    );
    await expect(bubble).toHaveCount(0);
    await expect(page.locator(".voice-stage")).toHaveText("");
    await expect.poll(() => lastSize(page)).toEqual(DROPLET_SIZE);
    await emit(page, {
      revision: 5,
      status: "error",
      error: "delivery_uncertain",
      draft: "Диктор отправляй дословно",
    });
    await expect(drop).toHaveAttribute("data-tone", "error");
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
      await bubble.evaluate((el) => el.scrollHeight <= el.clientHeight),
    ).toBe(true);
    await expect.poll(async () => (await lastSize(page))?.width).toBe(350);
    expect(
      await page.evaluate(() =>
        window.__voiceCalls.some((call) => call.command === "voice_copy_draft"),
      ),
    ).toBe(false);
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
      const box = await page.locator(".voice-bubble").boundingBox();
      expect(box?.height).toBeLessThanOrEqual(140);
    }
    expect(
      await page.evaluate(() =>
        window.__voiceCalls.some((call) => call.command === "voice_copy_draft"),
      ),
    ).toBe(false);
  });
}

test("loading, speech and live text show only the droplet; a draft keeps exact whitespace", async ({
  page,
}) => {
  await setup(page);
  const drop = page.locator(".voice-drop");
  const bubble = page.locator(".voice-bubble");
  await expect(drop).toHaveAttribute("data-tone", "neutral");
  await expect(drop).toHaveAttribute("aria-label", "Подключаю…");
  await expect(bubble).toHaveCount(0);
  await emit(page, { revision: 2, status: "listening", speech_active: true });
  await expect(drop).toHaveAttribute("data-tone", "speech");
  await expect(drop).toHaveAttribute("aria-label", "Слышу речь…");
  await expect(bubble).toHaveCount(0);
  await emit(page, {
    revision: 3,
    status: "listening",
    committed: "Первая  часть ",
    tentative: "и продолжение",
  });
  await expect(drop).toHaveAttribute("data-tone", "neutral");
  await expect(bubble).toHaveCount(0);
  await expect(page.locator(".voice-stage")).toHaveText("");
  await expect.poll(() => lastSize(page)).toEqual(DROPLET_SIZE);
  await emit(page, {
    revision: 4,
    status: "draft",
    error: "target_changed",
    draft: "Первая  часть и продолжение",
  });
  expect(await page.getByTestId("voice-text").textContent()).toBe(
    "Первая  часть и продолжение",
  );
  expect(
    await page
      .getByTestId("voice-text")
      .evaluate((el) => getComputedStyle(el).fontSize),
  ).toBe("14px");
  const box = await bubble.boundingBox();
  expect(box?.width).toBe(320);
  expect(box?.height).toBeLessThanOrEqual(140);
});

test("stale generation, phrase and revision cannot revive a draft, the tone or a stopped mode", async ({
  page,
}) => {
  await setup(page);
  const drop = page.locator(".voice-drop");
  await emit(page, {
    revision: 10,
    phrase: 2,
    status: "draft",
    error: "target_changed",
    draft: "Новая фраза",
  });
  for (const stale of [
    { generation: 3, revision: 100, phrase: 3 },
    { phrase: 1, revision: 11 },
    { phrase: 2, revision: 9 },
  ]) {
    await emit(page, {
      status: "draft",
      error: "target_changed",
      draft: "Старое",
      ...stale,
    });
    await emit(page, { status: "listening", speech_active: true, ...stale });
  }
  await expect(page.getByTestId("voice-text")).toHaveText("Новая фраза");
  await expect(drop).toHaveAttribute("data-tone", "error");
  await emit(page, {
    revision: 11,
    phrase: 2,
    status: "listening",
    speech_active: true,
  });
  await expect(drop).toHaveAttribute("data-tone", "speech");
  await emit(page, {
    revision: 11,
    phrase: 2,
    status: "error",
    error: "backlog",
    draft: "Старое",
  });
  await expect(drop).toHaveAttribute("data-tone", "speech");
  await emit(page, { revision: 12, phrase: 2, status: "off" });
  await emit(page, {
    revision: 10,
    phrase: 2,
    status: "draft",
    error: "target_changed",
    draft: "Поздний ответ",
  });
  await expect(page.locator(".voice-stage")).toBeHidden();
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
  const box = await page.locator(".voice-bubble").boundingBox();
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

test("commit stays droplet-sized and a fault grows the window to a bounded bubble", async ({
  page,
}) => {
  await setup(page);
  await emit(page, {
    revision: 2,
    status: "committing",
    committed: "Фраза перед вставкой",
  });
  await expect(page.locator(".voice-drop")).toHaveAttribute(
    "data-tone",
    "sending",
  );
  await expect(page.locator(".voice-bubble")).toHaveCount(0);
  await expect.poll(() => lastSize(page)).toEqual(DROPLET_SIZE);
  const beforeFault = await sizes(page);
  expect(beforeFault.every((size) => size.width === 24)).toBe(true);
  await emit(page, {
    revision: 3,
    status: "error",
    error: "backlog",
    draft: "Сохранённый текст",
  });
  await expect(page.getByTestId("voice-text")).toHaveText("Сохранённый текст");
  await expect(page.locator(".voice-status")).toContainText("Режим остановлен");
  await expect.poll(async () => (await lastSize(page))?.width).toBe(350);
  expect(
    (await sizes(page)).every(
      ({ width, height }) =>
        (width === 24 && height === 32) ||
        (width === 350 && Number(height) >= 32 && Number(height) <= 140),
    ),
  ).toBe(true);
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

/** Droplet box measured from the viewport's right and bottom edges. */
async function dropletFromCorner(page: Page) {
  return page.locator(".voice-drop").evaluate((el) => {
    const box = el.getBoundingClientRect();
    return {
      right: window.innerWidth - box.right,
      bottom: window.innerHeight - box.bottom,
      width: box.width,
      height: box.height,
    };
  });
}

test("droplet stays pixel-stable while the window is resized for the failure bubble", async ({
  page,
}) => {
  await setup(page);
  const drop = page.locator(".voice-drop");
  const bubble = page.locator(".voice-bubble");
  const draft = "Длинная фраза с сохранёнными пробелами. ".repeat(6);
  const states = [
    { change: { status: "loading" }, tone: "neutral", failure: false },
    { change: { status: "listening" }, tone: "neutral", failure: false },
    {
      change: { status: "listening", speech_active: true },
      tone: "speech",
      failure: false,
    },
    {
      change: {
        status: "listening",
        speech_active: true,
        committed: "Капля стоит на месте ",
        tentative: "и текста рядом нет",
      },
      tone: "speech",
      failure: false,
    },
    {
      change: { status: "committing", committed: "Капля стоит на месте" },
      tone: "sending",
      failure: false,
    },
    {
      change: { status: "error", error: "target_changed", draft },
      tone: "error",
      failure: true,
    },
    {
      change: { status: "draft", error: "desktop", draft: "Короткий" },
      tone: "error",
      failure: true,
    },
    { change: { status: "stopping" }, tone: "neutral", failure: false },
    { change: { status: "listening" }, tone: "neutral", failure: false },
  ] as const;
  let expected: Awaited<ReturnType<typeof dropletFromCorner>> | undefined;
  for (const [i, { change, tone, failure }] of states.entries()) {
    await emit(page, { revision: 2 + i, ...change });
    await expect(drop).toHaveAttribute("data-tone", tone);
    await expect(drop).toHaveAttribute("data-status", change.status);
    await expect(bubble).toHaveCount(failure ? 1 : 0);
    // Emulate the native window: the viewport becomes the size requested.
    const stage = await page
      .locator(".voice-stage")
      .evaluate((el) => el.getBoundingClientRect().toJSON());
    const size = { generation: 4, width: stage.width, height: stage.height };
    await expect.poll(() => lastSize(page)).toEqual(size);
    if (failure) expect(size.width).toBe(350);
    else expect(size).toEqual(DROPLET_SIZE);
    await page.setViewportSize({
      width: Math.ceil(size.width),
      height: Math.ceil(size.height),
    });
    const corner = await dropletFromCorner(page);
    expect(corner).toEqual({ right: 0, bottom: 0, width: 24, height: 32 });
    expected ??= corner;
    expect(corner).toEqual(expected);
    if (failure) {
      const box = await bubble.evaluate((el) =>
        el.getBoundingClientRect().toJSON(),
      );
      expect(box.left).toBe(0);
      expect(box.right).toBe(320);
      expect(box.bottom).toBe(Math.ceil(size.height));
    }
    // The resized window must not change the requested size (no resize loop).
    await page.evaluate(
      () => new Promise((resolve) => requestAnimationFrame(resolve)),
    );
    expect(await lastSize(page)).toEqual(size);
  }
});

test("failure bubble fades out with its last draft before the window shrinks", async ({
  page,
}) => {
  // Install before load: a real timer armed at mount cannot be cleared by the
  // fake clearTimeout and would end the fade before the test observes it.
  await page.clock.install();
  await setup(page);
  await emit(page, {
    revision: 2,
    status: "draft",
    error: "target_changed",
    draft: "Уходящий текст",
  });
  const bubble = page.locator(".voice-bubble");
  await expect(bubble).toHaveAttribute("data-leaving", "false");
  await expect.poll(async () => (await lastSize(page))?.width).toBe(350);
  expect(
    await bubble.evaluate((el) => getComputedStyle(el).animationName),
  ).toBe("voice-fade-in");
  await emit(page, { revision: 3, status: "listening" });
  await expect(bubble).toHaveAttribute("data-leaving", "true");
  await expect(page.getByTestId("voice-text")).toHaveText("Уходящий текст");
  expect(
    await bubble.evaluate((el) => getComputedStyle(el).transitionProperty),
  ).toBe("opacity");
  expect((await lastSize(page))?.width).toBe(350);
  await page.clock.runFor(150);
  await expect(bubble).toHaveCount(0);
  await expect.poll(() => lastSize(page)).toEqual(DROPLET_SIZE);
});

/** No countdown strip, nor any other element pinned to the stage's bottom edge. */
async function expectNoStrip(page: Page) {
  await expect(page.locator(".voice-silence, [data-kind]")).toHaveCount(0);
  expect(
    await page.locator(".voice-stage").evaluate((stage) =>
      [...stage.querySelectorAll("*")].some((el) => {
        const style = getComputedStyle(el);
        return style.position === "absolute" && style.bottom === "0px";
      }),
    ),
  ).toBe(false);
}

test("paste countdown draws no strip and snapshots preserve draft scroll", async ({
  page,
}) => {
  await setup(page);
  await page.emulateMedia({ reducedMotion: "reduce" });
  await page.clock.install();
  const committed = "Длинная фраза с сохранёнными пробелами. ".repeat(25);
  for (const [revision, remaining] of [
    [2, 1000],
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
    await expectNoStrip(page);
    await expect(page.locator(".voice-bubble")).toHaveCount(0);
    await page.clock.runFor(1500);
    await expectNoStrip(page);
  }
  await emit(page, {
    revision: 6,
    status: "draft",
    error: "target_changed",
    draft: committed,
  });
  const text = page.getByTestId("voice-text");
  await text.evaluate((el) => {
    el.scrollTop = 0;
  });
  for (const [revision, remaining] of [
    [7, 1000],
    [8, 0],
  ]) {
    await emit(page, {
      revision,
      status: "draft",
      error: "target_changed",
      draft: committed,
      silence_remaining_ms: remaining,
      send_remaining_ms: remaining,
    });
    await expectNoStrip(page);
    await page.clock.runFor(1500);
    expect(await text.evaluate((el) => el.scrollTop)).toBe(0);
    await expect(text).toHaveText(committed);
  }
  await emit(page, { generation: 5, revision: 1, status: "listening" });
  await expectNoStrip(page);
  await expect(page.locator(".voice-bubble")).toHaveCount(0);
  expect(await page.locator(".voice-drop").boundingBox()).toMatchObject({
    width: 24,
    height: 32,
  });
});

test("send countdown to Enter draws no strip and no hint", async ({ page }) => {
  await setup(page, "en");
  await emit(page, { revision: 2, status: "listening" });
  await expectNoStrip(page);
  for (const [revision, change] of [
    [3, { send_remaining_ms: 1500 }],
    [4, { send_remaining_ms: 0 }],
    [5, { speech_active: true, send_remaining_ms: 1500 }],
  ] as const) {
    await emit(page, { revision, status: "listening", ...change });
    await expectNoStrip(page);
  }
  await emit(page, {
    revision: 6,
    status: "listening",
    send_remaining_ms: 3000,
  });
  await expect(page.locator(".voice-hint")).toHaveCount(0);
  await emit(page, {
    revision: 7,
    status: "listening",
    committed: "next phrase",
    silence_remaining_ms: 250,
  });
  await expectNoStrip(page);
  await emit(page, {
    revision: 8,
    status: "committing",
    committed: "next phrase",
    send_remaining_ms: 3000,
  });
  await expectNoStrip(page);
});

test("droplet tone screenshots on a dark desktop", async ({ page }) => {
  await setup(page);
  await page.setViewportSize({ width: 460, height: 180 });
  await page.emulateMedia({ reducedMotion: "reduce" });
  await page.evaluate(() => {
    document.documentElement.dataset.theme = "dark";
    document.body.style.background = "#191b1f";
  });
  const drop = page.locator(".voice-drop");
  const dot = () =>
    drop.evaluate((el) => getComputedStyle(el, "::before").backgroundColor);
  const colours = new Set<string>();
  await emit(page, { revision: 2, status: "listening" });
  await expect(page.locator(".voice-bubble")).toHaveCount(0);
  colours.add(await dot());
  await page.screenshot({ path: "/tmp/diktor-drop-idle.png" });
  await emit(page, {
    revision: 3,
    status: "listening",
    speech_active: true,
    committed: "Капля осталась на месте, ",
    tentative: "а текст рядом не показывается",
  });
  await expect(drop).toHaveAttribute("data-tone", "speech");
  await expect(page.locator(".voice-bubble")).toHaveCount(0);
  expect(
    await drop.evaluate((el) => getComputedStyle(el, "::before").animationName),
  ).toBe("none");
  colours.add(await dot());
  await page.screenshot({ path: "/tmp/diktor-drop-speech.png" });
  await emit(page, {
    revision: 4,
    status: "committing",
    committed: "Капля осталась на месте",
  });
  await expect(drop).toHaveAttribute("data-tone", "sending");
  colours.add(await dot());
  await emit(page, {
    revision: 5,
    status: "draft",
    error: "target_changed",
    draft: "Капля осталась на месте",
  });
  await expect(drop).toHaveAttribute("data-tone", "error");
  await expect(page.locator(".voice-bubble")).toBeVisible();
  colours.add(await dot());
  await page.screenshot({ path: "/tmp/diktor-drop-error.png" });
  expect(colours.size).toBe(4);
});
