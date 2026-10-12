import { useTranslation } from "react-i18next"
import type { CodecSupport, VideoCodec } from "@/lib/ipc"

/**
 * What to tell about the codec chosen: why HEVC can't be picked on this computer, or, when it
 * is picked, that some Telegram clients don't play it inline. Nothing for H.264.
 */
export function CodecNote({
  codec,
  support,
}: {
  codec: VideoCodec
  support: CodecSupport | null
}) {
  const { t } = useTranslation()

  if (support && !support.hevc) {
    return (
      <p role="status" className="text-xs text-muted-foreground">
        {t(`codec.unavailable.${support.reason ?? "no_hevc_encoder"}`)}
      </p>
    )
  }

  if (codec === "hevc") {
    return (
      <p
        role="status"
        className="rounded-lg border border-warning/30 bg-warning/5 p-2 text-xs"
      >
        {t("codec.hevcWarning")}
      </p>
    )
  }

  return null
}
