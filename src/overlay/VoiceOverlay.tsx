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

/** Bubble fade duration; keep in sync with VoiceOverlay.css. */
const FADE_MS = 150;

const KNOWN_ERRORS = [
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

type Tone = "neutral" | "speech" | "sending" | "error";

function toneOf(snapshot: VoiceSnapshot): Tone {
  switch (snapshot.status) {
    case "listening":
      return snapshot.speech_active ? "speech" : "neutral";
    case "committing":
      return "sending";
    case "draft":
    case "error":
      return "error";
    default:
      return "neutral";
  }
}

/** What the failure bubble shows; kept after it hides so it can fade out intact. */
interface Bubble {
  generation: number;
  phrase: number;
  text: string;
  message: string;
  copyable: boolean;
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
  const stage = useRef<HTMLDivElement>(null);
  const text = useRef<HTMLDivElement>(null);
  const [copyState, setCopyState] = useState<"idle" | "copied" | "copyFailed">(
    "idle",
  );
  const generation = snapshot?.generation;
  const phrase = snapshot?.phrase;
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
    snapshot?.draft,
    snapshot?.status,
  ]);
  useLayoutEffect(() => {
    const element = stage.current;
    if (!element || generation === undefined) return;
    // The stage is droplet (+ gap + fixed-width bubble), so its size never
    // depends on the window size and reporting it cannot start a resize loop.
    const observer = new ResizeObserver(() => {
      const { width, height } = element.getBoundingClientRect();
      void invoke("voice_overlay_size", { generation, width, height }).catch(
        () => {},
      );
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, [generation, snapshot?.status]);

  let status = "";
  let current: Bubble | null = null;
  if (snapshot?.status === "draft" || snapshot?.status === "error") {
    const errorKey =
      snapshot.error && KNOWN_ERRORS.includes(snapshot.error)
        ? snapshot.error
        : "model";
    status = t(`voice.errors.${errorKey}`);
    // Normal dictation is shown only by the droplet colour; the bubble exists
    // for failures, where the draft and its copy button must stay reachable.
    current = {
      generation: snapshot.generation,
      phrase: snapshot.phrase,
      text: snapshot.draft || snapshot.committed,
      message: status,
      copyable: snapshot.draft.length > 0,
    };
  } else if (snapshot && snapshot.status !== "off") {
    status = t(
      `voice.${snapshot.status === "listening" && snapshot.speech_active ? "speech" : snapshot.status}`,
    );
  }
  const shown = current !== null;
  const last = useRef<Bubble | null>(null);
  if (current) last.current = current;
  const [lingering, setLingering] = useState(false);
  useEffect(() => {
    if (shown) {
      setLingering(true);
      return;
    }
    const timer = setTimeout(() => setLingering(false), FADE_MS);
    return () => clearTimeout(timer);
  }, [shown]);

  if (!snapshot || snapshot.status === "off") return null;
  const bubble =
    current ??
    (lingering && last.current?.generation === snapshot.generation
      ? last.current
      : null);
  const leaving = current === null && bubble !== null;

  return (
    <div ref={stage} className="voice-stage">
      {bubble && (
        <div className="voice-bubble" data-leaving={leaving} aria-live="polite">
          {bubble.text.length > 0 && (
            <div className="voice-text" ref={text} data-testid="voice-text">
              {bubble.text}
            </div>
          )}
          <div className="voice-status">{bubble.message}</div>
          <div className="voice-recovery">
            {bubble.copyable && (
              <button
                type="button"
                tabIndex={-1}
                onMouseDown={(event) => event.preventDefault()}
                onClick={() => {
                  const requested = identity.current;
                  void invoke("voice_copy_draft", {
                    generation: bubble.generation,
                    phrase: bubble.phrase,
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
                  generation: bubble.generation,
                }).catch(() => {});
              }}
            >
              {t("voice.dismiss")}
            </button>
            {copyState !== "idle" && <span>{t(`voice.${copyState}`)}</span>}
          </div>
        </div>
      )}
      <div
        className="voice-drop"
        data-tone={toneOf(snapshot)}
        data-status={snapshot.status}
        role="status"
        aria-label={status}
      />
    </div>
  );
}
