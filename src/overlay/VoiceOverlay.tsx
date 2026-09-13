import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useTranslation } from "react-i18next";
import { syncLanguageFromSettings } from "@/i18n";
import "./VoiceOverlay.css";

export interface VoiceSnapshot {
  generation: number;
  phrase: number;
  revision: number;
  status:
    | "loading"
    | "listening"
    | "committing"
    | "draft"
    | "error"
    | "stopping"
    | "off";
  committed: string;
  tentative: string;
  draft: string;
  error: string | null;
  speech_active: boolean;
  silence_remaining_ms: number | null;
  send_remaining_ms: number | null;
}

const EMPTY_LEVELS = [0, 0, 0, 0, 0];

function groupLevels(payload: unknown): number[] {
  const buckets = Array.isArray(payload) ? payload : [];
  return EMPTY_LEVELS.map((_, bar) => {
    const start = Math.floor((bar * 16) / 5);
    const end = Math.floor(((bar + 1) * 16) / 5);
    let sum = 0;
    for (let i = start; i < end; i++) {
      const level: unknown = buckets[i];
      if (typeof level === "number" && Number.isFinite(level))
        sum += Math.max(0, Math.min(1, level));
    }
    return sum / (end - start);
  });
}

export function useVoiceSession() {
  const [snapshot, setSnapshot] = useState<VoiceSnapshot | null>(null);
  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    const receive = (next: VoiceSnapshot | null) => {
      if (!next || disposed) return;
      setSnapshot((previous) => {
        if (
          previous &&
          (next.generation < previous.generation ||
            (next.generation === previous.generation &&
              (next.phrase < previous.phrase ||
                next.revision <= previous.revision)))
        )
          return previous;
        return next;
      });
    };
    void listen<VoiceSnapshot>("voice-session-state", ({ payload }) =>
      receive(payload),
    )
      .then(async (stop) => {
        if (disposed) {
          stop();
          return;
        }
        unlisten = stop;
        receive(await invoke<VoiceSnapshot | null>("voice_session_snapshot"));
      })
      .catch(() => {});
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);
  return snapshot;
}

export default function VoiceOverlay({
  snapshot,
}: {
  snapshot: VoiceSnapshot | null;
}) {
  const { t } = useTranslation();
  const card = useRef<HTMLDivElement>(null);
  const text = useRef<HTMLDivElement>(null);
  const [copyState, setCopyState] = useState<"idle" | "copied" | "copyFailed">(
    "idle",
  );
  const generation = snapshot?.generation;
  const phrase = snapshot?.phrase;
  const listening = snapshot?.status === "listening";
  const [levels, setLevels] = useState(EMPTY_LEVELS);
  useEffect(() => {
    setLevels(EMPTY_LEVELS);
    if (!listening) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    let redraw: ReturnType<typeof setTimeout> | undefined;
    let stale: ReturnType<typeof setTimeout> | undefined;
    let pending = EMPTY_LEVELS;
    let lastDraw = -Infinity;
    const draw = () => {
      redraw = undefined;
      lastDraw = performance.now();
      setLevels(pending);
    };
    void listen<unknown>("mic-level", ({ payload }) => {
      if (disposed) return;
      pending = groupLevels(payload);
      if (redraw === undefined) {
        const delay = Math.max(0, 1000 / 30 - (performance.now() - lastDraw));
        if (delay === 0) draw();
        else redraw = setTimeout(draw, delay);
      }
      clearTimeout(stale);
      stale = setTimeout(() => {
        clearTimeout(redraw);
        redraw = undefined;
        setLevels(EMPTY_LEVELS);
      }, 250);
    })
      .then((stop) => {
        if (disposed) stop();
        else unlisten = stop;
      })
      .catch(() => {});
    return () => {
      disposed = true;
      unlisten?.();
      clearTimeout(redraw);
      clearTimeout(stale);
    };
  }, [generation, listening]);
  const identity = useRef("");
  identity.current = `${generation}:${phrase}`;
  useEffect(() => {
    void syncLanguageFromSettings();
    setCopyState("idle");
  }, [generation, phrase]);
  useLayoutEffect(() => {
    if (text.current) text.current.scrollTop = text.current.scrollHeight;
  }, [
    generation,
    phrase,
    snapshot?.committed,
    snapshot?.tentative,
    snapshot?.draft,
    snapshot?.status,
  ]);
  useLayoutEffect(() => {
    const element = card.current;
    if (!element || generation === undefined) return;
    const observer = new ResizeObserver(() => {
      void invoke("voice_overlay_height", {
        generation,
        height: element.getBoundingClientRect().height,
      }).catch(() => {});
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, [generation, snapshot?.status]);

  if (!snapshot || snapshot.status === "off") return null;
  const recoverable =
    snapshot.status === "draft" || snapshot.status === "error";
  const committed = recoverable
    ? snapshot.draft || snapshot.committed
    : snapshot.committed;
  const tentative = recoverable ? "" : snapshot.tentative;
  const hasText = committed.length > 0 || tentative.length > 0;
  const isIdle =
    snapshot.status === "listening" && !snapshot.speech_active && !hasText;
  const statusKey =
    snapshot.status === "listening" && snapshot.speech_active
      ? "speech"
      : snapshot.status;
  const knownErrors = [
    "target_changed",
    "target_unavailable",
    "unsupported_desktop",
    "backlog",
    "model",
    "out_of_memory",
    "unsupported_model",
    "microphone",
    "timeout",
    "desktop",
    "busy",
    "cancelled",
    "delivery_uncertain",
  ];
  const errorKey =
    snapshot.error && knownErrors.includes(snapshot.error)
      ? snapshot.error
      : "model";
  const status = recoverable
    ? t(`voice.errors.${errorKey}`)
    : t(`voice.${statusKey}`);
  const ratio = (remaining: number | null | undefined, total: number) =>
    listening &&
    !snapshot.speech_active &&
    typeof remaining === "number" &&
    Number.isFinite(remaining)
      ? Math.max(0, Math.min(1, remaining / total))
      : null;
  // 1 s to paste the phrase; the send tail before Enter is only a few hundred ms.
  const pasteRatio = ratio(snapshot.silence_remaining_ms, 1000);
  const silenceRatio = pasteRatio ?? ratio(snapshot.send_remaining_ms, 3000);

  return (
    <div className="voice-stage">
      <div
        ref={card}
        className="voice-panel"
        data-status={snapshot.status}
        data-speech={snapshot.speech_active}
        role="status"
        aria-live="polite"
      >
        {listening ? (
          <span className="voice-levels" aria-hidden="true">
            {levels.map((level, index) => (
              <span key={index} style={{ height: `${2 + level * 12}px` }} />
            ))}
          </span>
        ) : (
          <span className="voice-dot" aria-hidden="true" />
        )}
        {hasText && (
          <div className="voice-text" ref={text} data-testid="voice-text">
            <span className="voice-committed">{committed}</span>
            <span className="voice-tentative">{tentative}</span>
          </div>
        )}
        {(!hasText || snapshot.status !== "listening") &&
          (isIdle ? (
            <div className="voice-idle-row">
              <div className="voice-status">{t("voice.listening")}</div>
              <div className="voice-hint">{t("voice.sendHint")}</div>
            </div>
          ) : (
            <div className="voice-status">{status}</div>
          ))}
        {recoverable && (
          <div className="voice-recovery">
            {snapshot.draft && (
              <button
                type="button"
                tabIndex={-1}
                onMouseDown={(event) => event.preventDefault()}
                onClick={() => {
                  const requested = identity.current;
                  void invoke("voice_copy_draft", {
                    generation: snapshot.generation,
                    phrase: snapshot.phrase,
                  })
                    .then(() => {
                      if (identity.current === requested)
                        setCopyState("copied");
                    })
                    .catch(() => {
                      if (identity.current === requested)
                        setCopyState("copyFailed");
                    });
                }}
              >
                {t("voice.copy")}
              </button>
            )}
            <button
              type="button"
              tabIndex={-1}
              onMouseDown={(event) => event.preventDefault()}
              onClick={() => {
                void invoke("voice_dismiss", {
                  generation: snapshot.generation,
                }).catch(() => {});
              }}
            >
              {t("voice.dismiss")}
            </button>
            {copyState !== "idle" && <span>{t(`voice.${copyState}`)}</span>}
          </div>
        )}
        {silenceRatio !== null && (
          <span
            className="voice-silence"
            data-kind={pasteRatio === null ? "send" : "paste"}
            aria-hidden="true"
            style={{ transform: `scaleX(${silenceRatio})` }}
          />
        )}
      </div>
    </div>
  );
}
