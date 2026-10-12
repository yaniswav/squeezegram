import { FlaskConical } from "lucide-react"
import { QRCodeSVG } from "qrcode.react"
import { useId, useState, type ReactNode } from "react"
import { useTranslation } from "react-i18next"
import { ConfirmDialog } from "@/components/confirm-dialog"
import { PageHeader } from "@/components/page-header"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import { useNow } from "@/hooks/use-now"
import { errorMessage } from "@/i18n/errors"
import { formatBytes, formatDuration } from "@/lib/format"
import {
  authLogout,
  authResetSession,
  authSubmit,
  type AuthInput,
  type AuthState,
} from "@/lib/ipc"
import { useAlerts } from "@/state/alerts"
import { useAuth } from "@/state/auth"

interface ConnectionPageProps {
  focusTitle: boolean
}

export function ConnectionPage({ focusTitle }: ConnectionPageProps) {
  const { t } = useTranslation()
  const { status, auth } = useAuth()

  return (
    <div className="space-y-6">
      <PageHeader title={t("connection.title")} focusTitle={focusTitle} />
      {auth?.transport === "mock" && (
        <p
          role="note"
          className="flex items-center gap-2 rounded-lg border border-dashed px-3 py-2 text-xs text-muted-foreground"
        >
          <FlaskConical className="size-3.5 shrink-0" aria-hidden />
          {t("connection.testTransport")}
        </p>
      )}
      <Card>
        <CardContent className="space-y-4">
          {status === "loading" && (
            <p className="text-sm text-muted-foreground">
              {t("common.loading")}
            </p>
          )}
          {status === "error" && auth === null && (
            <p className="text-sm text-destructive">
              {t("connection.loadError")}
            </p>
          )}
          {auth !== null && <AuthStep auth={auth} />}
        </CardContent>
      </Card>
    </div>
  )
}

/** The step of the login state machine, driven only by `auth://state`. */
function AuthStep({ auth }: { auth: AuthState }) {
  const { t } = useTranslation()

  switch (auth.state) {
    case "starting":
      return <Status text={t("connection.starting")} />
    case "logging_out":
      return <Status text={t("connection.loggingOut")} />
    case "closed":
      // Only a build without Telegram is closed for good; otherwise TDLib was stopped.
      return (
        <Status
          text={
            auth.transport === "none"
              ? t("connection.closed")
              : t("connection.stopped")
          }
        />
      )
    case "start_failed":
      return <StartFailedStep auth={auth} />
    case "wait_phone":
      return <PhoneStep />
    case "wait_qr":
      return <QrStep link={auth.qrLink} />
    case "wait_code":
      return <CodeStep key={auth.resendInS ?? "none"} auth={auth} />
    case "wait_email":
      return (
        <ValueStep
          kind="email"
          title={t("connection.email.title")}
          label={t("connection.email.label")}
          hint={t("connection.email.hint")}
          type="email"
          autoComplete="email"
        />
      )
    case "wait_email_code":
      return (
        <ValueStep
          kind="email_code"
          title={t("connection.emailCode.title")}
          label={t("connection.emailCode.label")}
          type="text"
          inputMode="numeric"
          autoComplete="one-time-code"
        />
      )
    case "wait_password":
      return (
        <ValueStep
          kind="password"
          title={t("connection.password.title")}
          label={t("connection.password.label")}
          hint={
            auth.passwordHint
              ? t("connection.password.hint", { hint: auth.passwordHint })
              : undefined
          }
          type="password"
          autoComplete="current-password"
        />
      )
    case "ready":
      return <ReadyStep auth={auth} />
    case "other_step":
      return (
        <div className="space-y-1">
          <h2 className="font-heading text-base font-medium">
            {t("connection.other.title")}
          </h2>
          <p className="text-sm text-muted-foreground">
            {/* The step name comes from the core and is not translated. */}
            {t("connection.other.body", { step: auth.otherStep ?? "?" })}
          </p>
        </div>
      )
  }
}

function Status({ text }: { text: string }) {
  return (
    <p role="status" className="text-sm text-muted-foreground">
      {text}
    </p>
  )
}

/** Sends one `auth_submit`; errors (a wrong code…) become alerts. */
function useSubmit() {
  const { pushError } = useAlerts()
  const [busy, setBusy] = useState(false)

  async function submit(input: AuthInput): Promise<boolean> {
    setBusy(true)

    try {
      await authSubmit(input)

      return true
    } catch (error) {
      pushError(error)

      return false
    } finally {
      setBusy(false)
    }
  }

  return { busy, submit }
}

interface ValueStepProps {
  kind: "phone" | "code" | "email" | "email_code" | "password"
  title: string
  label: string
  hint?: string
  type: "text" | "email" | "password" | "tel"
  inputMode?: "numeric" | "tel"
  autoComplete: string
  children?: ReactNode
}

/** One labelled field and a Continue button. */
function ValueStep({
  kind,
  title,
  label,
  hint,
  type,
  inputMode,
  autoComplete,
  children,
}: ValueStepProps) {
  const { t } = useTranslation()
  const { busy, submit } = useSubmit()
  const id = useId()
  const [value, setValue] = useState("")

  return (
    <form
      className="space-y-4"
      onSubmit={(event) => {
        event.preventDefault()

        if (value.trim() === "") return
        // Passwords are sent as typed; the other values lose stray spaces.
        void submit({
          kind,
          value: kind === "password" ? value : value.trim(),
        })
      }}
    >
      <h2 className="font-heading text-base font-medium">{title}</h2>
      <div className="space-y-2">
        <Label htmlFor={id}>{label}</Label>
        <Input
          id={id}
          type={type}
          inputMode={inputMode}
          autoComplete={autoComplete}
          // Phone numbers, codes and e-mail addresses are always left-to-right.
          dir="ltr"
          autoFocus
          value={value}
          aria-describedby={hint ? `${id}-hint` : undefined}
          onChange={(event) => setValue(event.target.value)}
        />
        {hint && (
          <p id={`${id}-hint`} className="text-xs text-muted-foreground">
            {hint}
          </p>
        )}
      </div>
      <div className="flex flex-wrap items-center gap-2">
        <Button type="submit" disabled={busy || value.trim() === ""}>
          {t("connection.submit")}
        </Button>
        {children}
      </div>
    </form>
  )
}

function PhoneStep() {
  const { t } = useTranslation()
  const { busy, submit } = useSubmit()

  return (
    <ValueStep
      kind="phone"
      title={t("connection.phone.title")}
      label={t("connection.phone.label")}
      hint={t("connection.phone.hint")}
      type="tel"
      inputMode="tel"
      autoComplete="tel"
    >
      <Button
        type="button"
        variant="ghost"
        disabled={busy}
        onClick={() => void submit({ kind: "request_qr" })}
      >
        {t("connection.phone.useQr")}
      </Button>
    </ValueStep>
  )
}

function QrStep({ link }: { link: string | undefined }) {
  const { t } = useTranslation()
  const [usePhone, setUsePhone] = useState(false)

  if (usePhone) return <PhoneStep />

  return (
    <div className="space-y-4">
      <h2 className="font-heading text-base font-medium">
        {t("connection.qr.title")}
      </h2>
      <p className="text-sm text-muted-foreground">{t("connection.qr.body")}</p>
      {link ? (
        // Always dark on white, whatever the theme: scanners need the contrast.
        <div className="w-fit rounded-xl bg-white p-3">
          <QRCodeSVG
            value={link}
            size={208}
            level="M"
            title={t("connection.qr.alt")}
          />
        </div>
      ) : (
        <Status text={t("common.loading")} />
      )}
      <Button variant="ghost" onClick={() => setUsePhone(true)}>
        {t("connection.qr.usePhone")}
      </Button>
    </div>
  )
}

function CodeStep({ auth }: { auth: AuthState }) {
  const { t, i18n } = useTranslation()
  const { busy, submit } = useSubmit()

  // `resendInS` is a snapshot: the deadline is fixed when this step appears.
  const [deadline] = useState(() =>
    auth.resendInS === undefined ? null : Date.now() + auth.resendInS * 1000,
  )

  const now = useNow(deadline !== null)
  const remaining = deadline === null ? 0 : Math.max(0, (deadline - now) / 1000)

  return (
    <ValueStep
      kind="code"
      title={t("connection.code.title")}
      label={t("connection.code.label")}
      hint={t("connection.code.hint")}
      type="text"
      inputMode="numeric"
      autoComplete="one-time-code"
    >
      <Button
        type="button"
        variant="outline"
        disabled={busy || remaining > 0}
        onClick={() => void submit({ kind: "resend_code" })}
      >
        {t("connection.code.resend")}
      </Button>
      {remaining > 0 && (
        <span className="text-xs text-muted-foreground tabular-nums">
          {t("connection.code.resendIn", {
            duration: formatDuration(remaining, i18n.language),
          })}
        </span>
      )}
    </ValueStep>
  )
}

/**
 * Telegram did not start: why, and when the local session itself is the cause (key refused
 * after a Windows password reset, a profile migration or a copy to another account), a way
 * out that deletes that session only.
 */
function StartFailedStep({ auth }: { auth: AuthState }) {
  const { t } = useTranslation()
  const { pushError } = useAlerts()
  const [confirming, setConfirming] = useState(false)
  const [busy, setBusy] = useState(false)

  async function reset() {
    setBusy(true)

    try {
      await authResetSession()
    } catch (error) {
      pushError(error)
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="space-y-4">
      <div className="space-y-1">
        <h2 className="font-heading text-base font-medium">
          {t("connection.startFailed.title")}
        </h2>
        {auth.error && (
          <p role="alert" className="text-sm text-destructive">
            {errorMessage(t, auth.error.code, auth.error.params)}
          </p>
        )}
      </div>
      {auth.canResetSession ? (
        <>
          <p className="text-sm text-muted-foreground">
            {t("connection.startFailed.resetHint")}
          </p>
          <Button
            variant="outline"
            disabled={busy}
            onClick={() => setConfirming(true)}
          >
            {t("connection.startFailed.reset")}
          </Button>
          <ConfirmDialog
            open={confirming}
            onOpenChange={setConfirming}
            title={t("connection.startFailed.resetTitle")}
            description={t("connection.startFailed.resetBody")}
            confirmLabel={t("connection.startFailed.resetConfirm")}
            cancelLabel={t("common.cancel")}
            destructive
            onConfirm={() => void reset()}
          />
        </>
      ) : (
        <p className="text-sm text-muted-foreground">
          {t("connection.startFailed.otherHint")}
        </p>
      )}
    </div>
  )
}

function ReadyStep({ auth }: { auth: AuthState }) {
  const { t, i18n } = useTranslation()
  const { pushError } = useAlerts()
  const [confirming, setConfirming] = useState(false)
  const [busy, setBusy] = useState(false)
  const me = auth.me

  async function logout() {
    setBusy(true)

    try {
      await authLogout()
    } catch (error) {
      pushError(error)
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="space-y-4">
      <div className="space-y-1">
        <p className="text-sm text-muted-foreground">
          {t("connection.ready.title")}
        </p>
        {me && (
          <div className="flex flex-wrap items-center gap-2">
            <h2 className="font-heading text-base font-medium">
              <bdi>{me.displayName}</bdi>
            </h2>
            {me.username && (
              <span className="text-sm text-muted-foreground" dir="ltr">
                @{me.username}
              </span>
            )}
            <Badge variant={me.isPremium ? "default" : "secondary"}>
              {me.isPremium
                ? t("connection.ready.premium")
                : t("connection.ready.standard")}
            </Badge>
          </div>
        )}
      </div>
      {me && (
        <ul className="space-y-1 text-sm text-muted-foreground">
          <li>
            {t("connection.ready.limit", {
              size: formatBytes(me.uploadLimitBytes, i18n.language),
            })}
          </li>
          <li>
            {t("connection.ready.caption", { count: me.captionMaxChars })}
          </li>
        </ul>
      )}
      <Button
        variant="outline"
        disabled={busy}
        onClick={() => setConfirming(true)}
      >
        {t("connection.ready.logout")}
      </Button>
      <ConfirmDialog
        open={confirming}
        onOpenChange={setConfirming}
        title={t("connection.ready.logoutTitle")}
        description={t("connection.ready.logoutBody")}
        confirmLabel={t("connection.ready.logoutConfirm")}
        cancelLabel={t("common.cancel")}
        destructive
        onConfirm={() => void logout()}
      />
    </div>
  )
}
