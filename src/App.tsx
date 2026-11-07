"use client"
import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type FormEvent,
  type ReactNode,
} from "react"
import {
  ACTIONS,
  ApiError,
  COMPOSITION_ACTIONS,
  COMPOSITION_TEMPLATES,
  TEMPLATES,
  createApi,
  type Action,
  type AiStatus,
  type CaseDetail,
  type CasePage,
  type CaseSummary,
  type RecordEntry,
  type SatisfactionItem,
  type SyncedHistoryEntry,
  type ReviewApi,
  type ServerAutoPatch,
  type ServerAutoStatus,
  type Session,
  type Suggestion,
} from "./lib/api"
import { createDemoApi } from "./lib/demo"

const TOKEN_KEY = "qingfeng.access-token"
const DEMO_KEY = "qingfeng.demo"
/**
 * 安卓客户端由壳带 `?client=android` 打开：它的后台进程随时会被系统回收，
 * 所以值守/自动审核这类需要常驻的能力在这里一律置灰，而不是假装能用。
 */
const MOBILE_SHELL =
  typeof window !== "undefined" &&
  new URLSearchParams(window.location.search).get("client") === "android"
const EXPIRED_MESSAGE = "社区凭证已失效，请重新粘贴 access-token"
/** 审核员社区 ID：设置里可手动填写，评论/作品历史同步共用。 */
const REVIEWER_ID_KEY = "qingfeng.reviewer-id"
const REVIEW_MODE_KEY = "qingfeng.review-mode"
type ReviewMode = "comment" | "composition"

function loadReviewerId(): number {
  try {
    const value = Number(localStorage.getItem(REVIEWER_ID_KEY) || 0)
    return Number.isFinite(value) && value > 0 ? Math.floor(value) : 0
  } catch {
    return 0
  }
}

function saveReviewerId(id: number) {
  try {
    if (id > 0) localStorage.setItem(REVIEWER_ID_KEY, String(id))
    else localStorage.removeItem(REVIEWER_ID_KEY)
  } catch {}
}

function loadReviewMode(): ReviewMode {
  try {
    return localStorage.getItem(REVIEW_MODE_KEY) === "composition"
      ? "composition"
      : "comment"
  } catch {
    return "comment"
  }
}

function saveReviewMode(mode: ReviewMode) {
  try {
    localStorage.setItem(REVIEW_MODE_KEY, mode)
  } catch {}
}

type Mode = "ai" | "manual"
type Tone = "success" | "error"
type FormState = {
  action: Action | null
  remark: string
  userMessage: string
  reporterMessage: string
}
type SuggestionState = { status: "idle" } | { status: "loading" } | {
  status: "ready"
  data: Suggestion
} | { status: "error"; message: string }

const EMPTY_FORM: FormState = {
  action: null,
  remark: "",
  userMessage: "",
  reporterMessage: "",
}

type AutoSettings = {
  enabled: boolean
  autoSubmit: boolean
  preferDelete: boolean
  interval: number
  threshold: number
}
type AutoStatus = {
  state: "idle" | "running" | "waiting" | "error"
  message: string
  lastRunAt: number | null
  submitted: number
  waiting: number
  failed: number
  cooling: number
  candidates: number
  stale: number
}

const DEFAULT_AUTO_STATUS: AutoStatus = {
  state: "idle",
  message: "等待首次检查",
  lastRunAt: null,
  submitted: 0,
  waiting: 0,
  failed: 0,
  cooling: 0,
  candidates: 0,
  stale: 0,
}
const AUTO_KEY = "qingfeng.auto"
const PENDING_KEY = "qingfeng.pending"
const AUTO_PER_TICK = 3
const AUTO_TICK_LIMIT = 120_000
const AUTO_MAX_ATTEMPTS = 5
// 拉取间隔可在设置里自定义（秒），超出范围时收敛到这个区间。
const AUTO_INTERVAL_MIN = 10
const AUTO_INTERVAL_MAX = 3600
const clampInterval = (seconds: number) =>
  Math.min(
    AUTO_INTERVAL_MAX,
    Math.max(AUTO_INTERVAL_MIN, Math.round(seconds || DEFAULT_AUTO.interval)),
  )
const AUTO_THRESHOLDS = [0.5, 0.6, 0.75]
const DEFAULT_AUTO: AutoSettings = {
  enabled: true,
  autoSubmit: true,
  preferDelete: true,
  interval: 60,
  threshold: 0.6,
}
function loadAuto(): AutoSettings {
  try {
    const raw = localStorage.getItem(AUTO_KEY)
    if (!raw) return DEFAULT_AUTO
    const stored = {
      ...DEFAULT_AUTO,
      ...JSON.parse(raw) as Partial<AutoSettings>,
    }
    stored.interval = clampInterval(stored.interval)
    if (!AUTO_THRESHOLDS.includes(stored.threshold))
      stored.threshold = DEFAULT_AUTO.threshold
    stored.enabled = DEFAULT_AUTO.enabled
    // 自动提交同样是主流程：旧版本存下的「关闭」会让每个案件都掉进待确认，这里每次启动重置。
    stored.autoSubmit = DEFAULT_AUTO.autoSubmit
    return stored
  } catch {
    return DEFAULT_AUTO
  }
}
/** 待人工确认的案件队列，持久化后刷新页面也不会丢。 */
function loadPending(): number[] {
  try {
    const raw = localStorage.getItem(PENDING_KEY)
    const parsed = raw ? JSON.parse(raw) : []
    return Array.isArray(parsed)
      ? parsed.filter((id): id is number => typeof id === "number")
      : []
  } catch {
    return []
  }
}

/** 今日工作量统计，按天存在 localStorage，跨天自动清零。 */
const STATS_KEY = "qingfeng.stats"
type DayStats = {
  date: string
  submitted: number
  waiting: number
  failed: number
  manual: number
}
const todayKey = () => new Date().toLocaleDateString("sv-SE")
const emptyStats = (date: string): DayStats => ({
  date,
  submitted: 0,
  waiting: 0,
  failed: 0,
  manual: 0,
})

function loadStats(): DayStats {
  const today = todayKey()
  try {
    const raw = localStorage.getItem(STATS_KEY)
    if (!raw) return emptyStats(today)
    const parsed = JSON.parse(raw) as Partial<DayStats>
    // 跨天就重新开始计数。
    if (!parsed || parsed.date !== today) return emptyStats(today)
    return { ...emptyStats(today), ...parsed, date: today }
  } catch {
    return emptyStats(today)
  }
}

/** 历史审核记录：只存本地，最多 50 条；满意度跟记录一起保存/删除。 */
const HISTORY_KEY = "qingfeng.history"
const HISTORY_LIMIT = 50
/** 刚提交的一天内满意度没意义，先等 24 小时。 */
const SATISFACTION_DELAY_MS = 24 * 60 * 60 * 1000
/** 只要记录还在，就每天重新取一次满意度，避免数据更新不及时。 */
const SATISFACTION_REFRESH_MS = 24 * 60 * 60 * 1000

/** 每轮最多刷新几条，避免一次发太多请求。 */
const SATISFACTION_BATCH = 10
/** 满意度分批请求同时在飞的数量：串行一批等一批是历史页慢的原因。 */
const SATISFACTION_CONCURRENCY = 3

/**
 * 限制并发的 map：不引第三方库，靠一个自增下标让几个 worker 轮流取任务。
 * 返回顺序与入参一致；单个任务失败由调用方在自己的 fn 里处理。
 */
async function mapWithConcurrency<T, R>(
  items: T[],
  limit: number,
  fn: (item: T) => Promise<R>,
): Promise<R[]> {
  const results = new Array<R>(items.length)
  let next = 0
  const workers = Array.from(
    { length: Math.max(1, Math.min(limit, items.length)) },
    async () => {
      while (next < items.length) {
        const index = next++
        results[index] = await fn(items[index])
      }
    },
  )
  await Promise.allSettled(workers)
  return results
}

type JudgeRecord = {
  caseId: number
  judgeRecordId: number
  nickname: string
  content: string
  action: Action | null
  at: number
  /** 满意度 0~1，null 表示还没取到或算不出来。 */
  satisfaction: number | null
  /** 取过至少一次（之后每天还会再刷）。 */
  satisfactionFetched: boolean
  /** 上次取满意度的时间；0 表示还没取过。 */
  satisfactionTriedAt: number
}

function loadHistory(): JudgeRecord[] {
  try {
    const raw = localStorage.getItem(HISTORY_KEY)
    const parsed = raw ? JSON.parse(raw) : []
    if (!Array.isArray(parsed)) return []
    return parsed
      .filter(
        (item): item is JudgeRecord =>
          Boolean(item) && typeof item.caseId === "number",
      )
      // 旧版本没有 satisfactionTriedAt，补 0 才不会算出 NaN。
      .map((item) => ({
        ...item,
        judgeRecordId: item.judgeRecordId ?? 0,
        satisfaction: item.satisfaction ?? null,
        satisfactionFetched: Boolean(item.satisfactionFetched),
        satisfactionTriedAt: item.satisfactionTriedAt ?? 0,
      }))
      .slice(0, HISTORY_LIMIT)
  } catch {
    return []
  }
}

function saveHistory(list: JudgeRecord[]) {
  try {
    localStorage.setItem(HISTORY_KEY, JSON.stringify(list.slice(0, HISTORY_LIMIT)))
  } catch {}
}

/** 已审核案件列表同步下来的历史：本地缓存 10 分钟，避免每次进历史页都扫一遍。 */
const SYNCED_HISTORY_KEY = "qingfeng.history.synced"
const SYNCED_HISTORY_TTL = 10 * 60 * 1000

function loadSyncedHistory(owner?: string): {
  at: number
  records: RecordEntry[]
} {
  try {
    const raw = localStorage.getItem(SYNCED_HISTORY_KEY)
    if (!raw) return { at: 0, records: [] }
    const parsed = JSON.parse(raw)
    // 换 token 后不能沿用上一个审核员的同步缓存。
    if ((owner ?? "") !== String(parsed.owner ?? "")) {
      return { at: 0, records: [] }
    }
    const records = Array.isArray(parsed.records)
      ? parsed.records.filter(
          (item: RecordEntry) =>
            item &&
            typeof item.caseId === "number" &&
            item.source === "synced",
        )
      : []
    return { at: Number(parsed.at) || 0, records }
  } catch {
    return { at: 0, records: [] }
  }
}

function saveSyncedHistory(
  owner: string | undefined,
  at: number,
  records: RecordEntry[],
) {
  try {
    localStorage.setItem(
      SYNCED_HISTORY_KEY,
      JSON.stringify({
        owner: owner ?? "",
        at,
        records: records.slice(0, HISTORY_LIMIT),
      }),
    )
  } catch {}
}

/** 已审核列表只有 id/内容/最后举报时间，处理结果未知，action 记为 0。 */
function mapSyncedEntry(item: SyncedHistoryEntry): RecordEntry {
  const createdAt =
    item.lastReportTime > 0 ? item.lastReportTime * 1000 : Date.now()
  return {
    caseId: item.caseId,
    judgeRecordId: 0,
    nickname: item.nickname,
    content: item.content,
    action: 0,
    remark: "",
    source: "synced",
    confidence: null,
    satisfaction: null,
    satisfactionFetched: false,
    satisfactionAt: 0,
    caseReportTime:
      item.lastReportTime > 0
        ? new Date(createdAt).toLocaleString("zh-CN", {
            year: "numeric",
            month: "2-digit",
            day: "2-digit",
            hour: "2-digit",
            minute: "2-digit",
          })
        : "",
    createdAt,
    tokenMasked: "",
    pending: false,
  }
}

/** 接口返回的已判决列表，兼容 list / records / data / 数组几种结构。 */
function judgedItems(payload: unknown): Record<string, unknown>[] {
  const candidates: unknown[] = [
    payload,
    (payload as Record<string, unknown> | null)?.list,
    (payload as Record<string, unknown> | null)?.records,
    (payload as Record<string, unknown> | null)?.data,
    (payload as Record<string, unknown> | null)?.rows,
  ]
  for (const candidate of candidates) {
    if (Array.isArray(candidate)) {
      return candidate.filter(
        (item): item is Record<string, unknown> =>
          Boolean(item) && typeof item === "object",
      )
    }
  }
  return []
}

function judgeIdOf(item: Record<string, unknown> | undefined): number {
  if (!item) return 0
  // 只认判决记录号；列表项的 id 是案件号，绝不能拿来当判决记录号。
  for (const key of ["judgeRecordId", "judgeId"]) {
    const raw = item[key]
    const value =
      typeof raw === "number" ? raw : typeof raw === "string" ? Number(raw) : NaN
    if (Number.isFinite(value) && value > 0) return Math.round(value)
  }
  return 0
}

/**
 * 从「我已判决」列表里找出本案件的判决记录号。
 * 列表项的 id 就是案件号（例如 id=57864 → judgeRecordId=41873），只接受精确相等：
 * 匹配不到就返回 0，宁可不取也不要挂到别的案件上。
 */
function judgeIdForCase(page: unknown, caseId: number): number {
  if (caseId <= 0) return 0
  for (const item of judgedItems(page)) {
    const raw = item.id ?? item.caseId
    const value =
      typeof raw === "number" ? raw : typeof raw === "string" ? Number(raw) : NaN
    if (!Number.isFinite(value) || Math.round(value) !== caseId) continue
    const id = judgeIdOf(item)
    if (id > 0) return id
  }
  return 0
}

const ACTION_TONE: Record<Action, string> = {  1: "tone-green",
  2: "tone-orange",
  3: "tone-red",
}

const actionLabel = (action: Action) =>
  ACTIONS.find(
    (item) =>
      item.value ===
      action,
  )?.label ??
  ""

/** 作品的受理结果只有「正常显示 / 下架」，标签和配色与评论不同。 */
const compositionActionLabel = (action: Action) =>
  COMPOSITION_ACTIONS.find(
    (item) =>
      item.value ===
      action,
  )?.label ?? actionLabel(action)
const compositionTone = (action: Action) =>
  action === 3 ? "tone-green" : action === 2 ? "tone-orange" : ACTION_TONE[action]
const messageOf = (error: unknown) =>
  error instanceof
  Error
    ? error.message
    : "发生未知错误"
function autoMessage(
  submitted: number,
  waiting: number,
  failed: number,
  expired: boolean,
  autoSubmit: boolean,
  unclaimedTotal: number,
  staleTotal: number,
) {
  if (expired) return "本轮超时已中止，下一轮继续"
  if (
    submitted ===
      0 &&
    waiting ===
      0 &&
    failed ===
      0
  ) {
    if (unclaimedTotal === 0) return "列表里没有未认领的案件"
    if (staleTotal >= unclaimedTotal)
      return `${staleTotal} 件未认领案件都超过一周，已不再审理`
    return `${unclaimedTotal} 件未认领案件已处理或在退避中，等下一轮`
  }
  const parts: string[] = []
  if (submitted) parts.push(`自动处理 ${submitted} 件`)
  if (waiting)
    parts.push(
      autoSubmit
        ? `转人工 ${waiting} 件`
        : `转人工 ${waiting} 件（自动提交已关闭）`,
    )
  if (failed) parts.push(`失败 ${failed} 件`)
  if (staleTotal > 0) parts.push(`${staleTotal} 件超过一周已忽略`)
  return parts.join(" · ")
}

const clockOf = (time: number | null) =>
  time
    ? new Date(time).toLocaleTimeString("zh-CN", {
        hour: "2-digit",
        minute: "2-digit",
        second: "2-digit",
      })
    : "--:--:--"
const isExpired = (error: unknown) =>
  error instanceof
    ApiError &&
  error.expired
const profileUrl = (userId: string) =>
  `https://world.xiaomawang.com/w/person/project/all/${encodeURIComponent(userId)}`
const sceneUrl = (compositionId: string) =>
  `https://world.xiaomawang.com/community/main/compose/${encodeURIComponent(compositionId)}`
const isTemplate = (value: string, key: "user" | "reporter") =>
  !value.trim() ||
  [...Object.values(TEMPLATES), ...Object.values(COMPOSITION_TEMPLATES)].some(
    (template) =>
      template[key] ===
      value.trim(),
  )
const unclaimed = (checkerName: string) => {
  // 同一案件可能有多条举报记录，checkerName 会是「甲,乙」甚至「-,张三」这种拼接串：
  // 认领只看第一个出现的名字，与服务端 auto::first_unclaimed 保持一致。
  const first = checkerName.split(/[,，;；/|\n\t]/)[0].trim()
  // 社区接口用空字符串或各种横线（-、--、—）表示未认领。
  return first === "" || /^[-—–\s]+$/.test(first)
}

/** 超过这么久没人处理的案件不再自动审理。 */
const CASE_MAX_AGE_MS = 7 * 24 * 60 * 60 * 1000

/** 宽松解析社区接口的时间字符串（"2026-01-14 12:20:31"、"26-1-14 12:20"）。 */
function parseWhen(text?: string | null): number | null {
  if (!text) return null
  const match = /(\d{2,4})\s*[-/年]\s*(\d{1,2})\s*[-/月]\s*(\d{1,2})/.exec(text)
  if (!match) return null
  const rawYear = Number(match[1])
  const year = rawYear < 100 ? 2000 + rawYear : rawYear
  const month = Number(match[2])
  const day = Number(match[3])
  const time = /(\d{1,2}):(\d{2})/.exec(text)
  const value = new Date(
    year,
    month - 1,
    day,
    time ? Number(time[1]) : 0,
    time ? Number(time[2]) : 0,
  ).getTime()
  return Number.isFinite(value) ? value : null
}

/** 判决案件是否已经放置超过一周。 */
const isStale = (when: number | null) =>
  when !== null && Date.now() - when > CASE_MAX_AGE_MS

/** 举报时间缺失/无法解析：无法确认是否超期，按“不可审理”处理。 */
const timeUnknown = (text?: string | null) => parseWhen(text) === null

/** 列表时间可用且未超期，才进入自动审核候选。 */
const actionable = (item: CaseSummary) => {
  const when = parseWhen(item.createdAt)
  return when !== null && !isStale(when)
}

/** 用详情里的举报时间（取最早一条）判断案件年龄。
 *  口径是"举报人举报的时间"，绝不回退到被举报评论的发布时间。 */
function caseAgeOf(detail: CaseDetail): number | null {
  const times = detail.reports
    .map((report) => parseWhen(report.createdAt))
    .filter((value): value is number => value !== null)
  if (!times.length) return null
  return Math.min(...times)
}

type IconName = "tray" | "book" | "refresh" | "search" | "back" | "prev" | "next" | "external" | "sparkle" | "logout" | "check" | "alert" | "key" | "user" | "shield" | "gear" | "bolt" | "clock" | "close" | "trash" | "menu"

const ICONS: Record<IconName, ReactNode> = {
  menu: (
    <>
      <path d="M4 7h16" />
      <path d="M4 12h16" />
      <path d="M4 17h16" />
    </>
  ),
  tray: (
    <>
      <path d="M4 13.5V6.8A2.8 2.8 0 0 1 6.8 4h10.4A2.8 2.8 0 0 1 20 6.8v10.4a2.8 2.8 0 0 1-2.8 2.8H6.8A2.8 2.8 0 0 1 4 17.2v-3.7Z" />
      <path d="M4 13.5h4.2l1.3 2.2h5l1.3-2.2H20" />
    </>
  ),
  book: (
    <path d="M12 6.5C10.3 5.2 7.8 4.6 4.5 4.8v13c3.3-.2 5.8.4 7.5 1.7m0-13c1.7-1.3 4.2-1.9 7.5-1.7v13c-3.3-.2-5.8.4-7.5 1.7m0-13v13" />
  ),
  refresh: (
    <>
      <path d="M19.5 8.5A8 8 0 1 0 20 13" />
      <path d="M20 4.5v4h-4" />
    </>
  ),
  search: (
    <>
      <circle cx="10.8" cy="10.8" r="6.3" />
      <path d="m15.5 15.5 4 4" />
    </>
  ),
  back: <path d="m14.5 5-7 7 7 7" />,
  prev: <path d="m14 6-6 6 6 6" />,
  next: <path d="m10 6 6 6-6 6" />,
  external: (
    <>
      <path d="M13.5 4.5h6v6" />
      <path d="m19.5 4.5-8.5 8.5" />
      <path d="M17.5 14v3.7a1.8 1.8 0 0 1-1.8 1.8H6.3a1.8 1.8 0 0 1-1.8-1.8V8.3a1.8 1.8 0 0 1 1.8-1.8H10" />
    </>
  ),
  sparkle: (
    <path d="M12 3.5c.6 4.6 3.9 7.9 8.5 8.5-4.6.6-7.9 3.9-8.5 8.5-.6-4.6-3.9-7.9-8.5-8.5 4.6-.6 7.9-3.9 8.5-8.5Z" />
  ),
  logout: (
    <>
      <path d="M10 4.5H6.8A2.3 2.3 0 0 0 4.5 6.8v10.4a2.3 2.3 0 0 0 2.3 2.3H10" />
      <path d="M14.5 8 19 12l-4.5 4M19 12H9.5" />
    </>
  ),
  check: <path d="m5.5 12.5 4 4 9-9" />,
  alert: (
    <>
      <circle cx="12" cy="12" r="8.5" />
      <path d="M12 7.8v5M12 16.2h.01" />
    </>
  ),
  key: (
    <>
      <circle cx="8.5" cy="15.5" r="4" />
      <path d="m11.5 12.5 8-8M16.5 7.5l2 2" />
    </>
  ),
  user: (
    <>
      <circle cx="12" cy="8.5" r="3.4" />
      <path d="M5.5 19c.9-3.2 3.4-5 6.5-5s5.6 1.8 6.5 5" />
    </>
  ),
  shield: (
    <>
      <path d="M12 3.8 18.8 6.3v5.2c0 4.2-2.8 7.4-6.8 8.7-4-1.3-6.8-4.5-6.8-8.7V6.3L12 3.8Z" />
      <path d="m9 12 2.1 2.1L15.2 10" />
    </>
  ),
  gear: (
    <>
      <path d="M5 7h14M5 12h14M5 17h14" />
      <circle cx="9" cy="7" r="2" />
      <circle cx="15" cy="12" r="2" />
      <circle cx="9" cy="17" r="2" />
    </>
  ),
  bolt: <path d="M13 3.5 5.5 13.2h5l-1 7.3 7.5-9.7h-5l1-7.3Z" />,
  clock: (
    <>
      <circle cx="12" cy="12" r="8.5" />
      <path d="M12 7.5V12l3 1.8" />
    </>
  ),
  close: <path d="m6 6 12 12M18 6 6 18" />,
  trash: (
    <>
      <path d="M4.5 6.5h15M9.5 6.5V4.8h5v1.7M6.5 6.5l.8 12.2h9.4l.8-12.2" />
      <path d="M10 10.5v4.5M14 10.5v4.5" />
    </>
  ),
}

function Icon({
  name,
  size = 18,
  strokeWidth = 1.7,
}: {
  name: IconName
  size?: number
  strokeWidth?: number
}) {
  return (
    <svg
      aria-hidden="true"
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={strokeWidth}
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      {ICONS[name]}
    </svg>
  )
}

function AppMark({ large = false }: { large?: boolean }) {
  return (
    <span className={`app-mark ${large ? "large" : ""}`}>
      {/* 加粗描边，让盾牌在小尺寸（侧边栏/任务栏）也清晰可辨 */}
      <Icon name="shield" size={large ? 36 : 20} strokeWidth={2} />
    </span>
  )
}
function hueOf(name: string) {
  let hash = 0
  for (const char of Array.from(name))
    hash =
      (hash * 31 +
        (char.codePointAt(0) ??
          0)) %
      360
  return hash
}

function Avatar({
  src,
  name,
  size = "md",
}: {
  src?: string
  name: string
  size?: "sm" | "md"
}) {
  const [failed, setFailed] = useState(false)
  const label = name.trim()
  const initial =
    Array.from(label)[0] ??
    "用"
  const showImage =
    Boolean(src) &&
    !failed
  const hue = hueOf(
    label ||
      initial,
  )
  return (
    <span
      className={`avatar avatar-${size}`}
      style={
        showImage
          ? undefined
          : {
              backgroundImage: `linear-gradient(160deg, hsl(${hue} 62% 62%), hsl(${(hue + 38) % 360} 58% 46%))`,
            }
      }
    >
      {showImage ? (
        <img src={src} alt="" loading="lazy" onError={() => setFailed(true)} />
      ) : (
        <span className="avatar-initial">{initial}</span>
      )}
    </span>
  )
}

function Segmented<T extends string | number>({
  value,
  options,
  onChange,
  label,
  disabled = false,
}: {
  value: T | null
  options: { value: T; label: string }[]
  onChange: (value: T) => void
  label: string
  disabled?: boolean
}) {
  return (
    <div className="segmented" role="radiogroup" aria-label={label}>
      {options.map((option) => (
        <button
          type="button"
          key={option.value}
          role="radio"
          disabled={disabled}
          aria-checked={
            value ===
            option.value
          }
          className={
            value ===
            option.value
              ? "selected"
              : ""
          }
          onClick={() => onChange(option.value)}
        >
          {option.label}
        </button>
      ))}
    </div>
  )
}

function EmptyState({
  icon,
  title,
  text,
  action,
}: {
  icon: IconName
  title: string
  text: string
  action?: ReactNode
}) {
  return (
    <div className="empty-state">
      <Icon name={icon} size={36} />
      <h3>{title}</h3>
      <p>{text}</p>
      {action}
    </div>
  )
}

function Group({
  title,
  aside,
  children,
}: {
  title: string
  aside?: ReactNode
  children: ReactNode
}) {
  return (
    <section className="group">
      <header className="group-header">
        <h2>{title}</h2>
        {aside}
      </header>
      <div className="group-body">{children}</div>
    </section>
  )
}

function ExternalLink({
  href,
  children,
}: {
  href: string
  children: ReactNode
}) {
  return (
    <a className="link-pill" href={href} target="_blank" rel="noreferrer">
      {children}
      <Icon name="external" size={13} />
    </a>
  )
}

/** 评论 / 作品切换：两套模式共用同一个控件，白色指示块随 --mode-i 滑动。 */
function ModeSwitch({
  mode,
  onSwitch,
}: {
  mode: ReviewMode
  onSwitch: (mode: ReviewMode) => void
}) {
  return (
    <div
      className="mode-switch"
      style={{ "--mode-i": mode === "comment" ? 0 : 1 } as CSSProperties}
      role="group"
      aria-label="审核模式"
    >
      <span className="mode-thumb" aria-hidden="true" />
      <button
        type="button"
        className={mode === "comment" ? "active" : ""}
        aria-pressed={mode === "comment"}
        title="评论审核"
        onClick={() => onSwitch("comment")}
      >
        评论
      </button>
      <button
        type="button"
        className={mode === "composition" ? "active" : ""}
        aria-pressed={mode === "composition"}
        title="作品审核"
        onClick={() => onSwitch("composition")}
      >
        作品
      </button>
    </div>
  )
}

export default function App() {
  const [ready, setReady] = useState(false)
  const [auth, setAuth] = useState<{
    api: ReviewApi
    session: Session
    demo: boolean
  } | null>(null)
  const [notice, setNotice] = useState("")
  // 审核员 ID 与审核模式都是本机设置：评论 / 作品两套数据互不干扰。
  const [reviewerId, setReviewerId] = useState(loadReviewerId)
  const [reviewMode, setReviewMode] = useState<ReviewMode>(loadReviewMode)
  // 评论 / 作品两套子树都保持挂载：切走的一侧只是视觉隐藏，
  // 否则每次切换都要重新发一次 60 秒的列表请求，选中项和滚动位置也会丢。
  // 只有真正访问过的模式才挂载，首屏成本不变。
  const [visited, setVisited] = useState<Record<ReviewMode, boolean>>(() => ({
    comment: reviewMode === "comment",
    composition: reviewMode === "composition",
  }))
  const updateReviewerId = useCallback((id: number) => {
    saveReviewerId(id)
    setReviewerId(id)
  }, [])
  const switchReviewMode = useCallback((mode: ReviewMode) => {
    saveReviewMode(mode)
    setReviewMode(mode)
    setVisited((current) => (current[mode] ? current : { ...current, [mode]: true }))
  }, [])

  const logout = useCallback((message = "") => {
    localStorage.removeItem(TOKEN_KEY)
    localStorage.removeItem(SYNCED_HISTORY_KEY)
    sessionStorage.removeItem(DEMO_KEY)
    setAuth(null)
    setNotice(message)
  }, [])

  const start = useCallback(async (api: ReviewApi, demo: boolean) => {
    const session = await api.session()
    if (demo) sessionStorage.setItem(DEMO_KEY, "1")
    setNotice("")
    setAuth({ api, session, demo })
  }, [])

  useEffect(() => {
    const token = localStorage.getItem(TOKEN_KEY) ?? sessionStorage.getItem(TOKEN_KEY)
    const demo = sessionStorage.getItem(DEMO_KEY) === "1"
    const restore = demo
      ? start(createDemoApi(), true)
      : token
        ? start(createApi({ token }), false)
        : Promise.resolve()
    restore
      .catch((error) => {
        if (token) localStorage.removeItem(TOKEN_KEY)
        logout(error instanceof ApiError && error.status === 401 ? "" : messageOf(error))
      })
      .finally(() => setReady(true))
  }, [start, logout])

  if (!ready) {
    return (
      <div className="splash" aria-busy="true">
        <span className="splash-spinner" aria-hidden="true" />
        <span className="splash-text">正在校验 access-token…</span>
      </div>
    )
  }
  if (!auth) {
    return (
      <Login
        notice={notice}
        onGuest={(token) => {
          localStorage.setItem(TOKEN_KEY, token)
          sessionStorage.removeItem(DEMO_KEY)
          return start(createApi({ token }), false)
        }}
        onDemo={() => start(createDemoApi(), true)}
      />
    )
  }
  // 两套模式共用一个舞台：切走的一侧只淡出并禁用交互，子树不卸载。
  return (
    <div className="mode-stage" data-mode={reviewMode}>
      {visited.comment && (
        <div
          className={`mode-pane ${reviewMode === "comment" ? "active" : ""}`}
          aria-hidden={reviewMode !== "comment"}
          inert={reviewMode !== "comment"}
        >
          <Console
            {...auth}
            active={reviewMode === "comment"}
            reviewerId={reviewerId}
            onReviewerIdChange={updateReviewerId}
            onModeChange={switchReviewMode}
            onLogout={logout}
          />
        </div>
      )}
      {visited.composition && (
        <div
          className={`mode-pane ${
            reviewMode === "composition" ? "active" : ""
          }`}
          aria-hidden={reviewMode !== "composition"}
          inert={reviewMode !== "composition"}
        >
          <CompositionApp
            api={auth.api}
            session={auth.session}
            demo={auth.demo}
            active={reviewMode === "composition"}
            reviewerId={reviewerId}
            onReviewerIdChange={updateReviewerId}
            onModeChange={switchReviewMode}
            onLogout={logout}
          />
        </div>
      )}
    </div>
  )
}

function Login({
  notice,
  onGuest,
  onDemo,
}: {
  notice: string
  onGuest: (token: string) => Promise<void>
  onDemo: () => Promise<void>
}) {
  const [token, setToken] = useState("")
  const [error, setError] = useState(notice)
  const [pending, setPending] = useState(false)

  async function submit(event: FormEvent) {
    event.preventDefault()
    setError("")
    const value = token.trim()
    if (!value) return setError("请输入 access-token")
    setPending(true)
    try {
      await onGuest(value)
    } catch (submitError) {
      setError(
        isExpired(submitError) ? "Token 无效或已过期，请重新输入" : messageOf(submitError),
      )
      setPending(false)
    }
  }

  return (
    <main className="login">
      <form className="login-card" onSubmit={submit}>
        <AppMark large />
        <h1>风纪审核</h1>
        <p className="login-subtitle">使用社区 access-token 登录</p>

        <label className="field-label" htmlFor="token">
          Access Token
        </label>
        <div className="token-field">
          <Icon name="key" size={16} />
          <input
            id="token"
            type="password"
            autoComplete="off"
            spellCheck={false}
            placeholder="粘贴 access-token"
            value={token}
            onChange={(event) => setToken(event.target.value)}
          />
        </div>

        {error && (
          <p className="form-error" role="alert">
            <Icon name="alert" size={15} />
            {error}
          </p>
        )}
        <button className="btn btn-primary btn-block" disabled={pending}>
          {pending ? "正在验证…" : "进入审核台"}
        </button>

        <p className="login-note">
          开源版没有账号体系：社区 access-token 就是唯一凭证，审核记录与偏好按 token
          归属，只存在服务端 data 目录里。换浏览器需要重新粘贴。
        </p>
        <button
          type="button"
          className="link-button"
          onClick={() => {
            setError("")
            void onDemo()
          }}
        >
          使用示例数据预览
        </button>
        <p className="login-legal">
          <a href="https://beian.miit.gov.cn/" target="_blank" rel="noreferrer">
            皖ICP备2026007802号-1
          </a>
        </p>
      </form>
    </main>
  )
}

function Console({
  api,
  session,
  demo,
  active,
  reviewerId,
  onReviewerIdChange,
  onModeChange,
  onLogout,
}: {
  api: ReviewApi
  session: Session
  demo: boolean
  /** 当前显示的是不是评论模式：隐藏时不跑后台轮询，但保留已加载的数据。 */
  active: boolean
  /** 设置里手动填写的审核员社区 ID（历史同步用）。 */
  reviewerId: number
  onReviewerIdChange: (id: number) => void
  /** 切换评论 / 作品审核模式。 */
  onModeChange: (mode: ReviewMode) => void
  onLogout: (message?: string) => void
}) {
  // 开源版没有账号与角色：能用哪些功能只取决于服务端配置（AI 密钥是否填了）。
  const canUseAi = session.aiEnabled
  // 本机缓存按 token 掩码分区，换 token 时不会串到上一个审核员的数据。
  const owner = session.community?.masked ?? ""
  // 窄屏（手机/平板）侧边栏是抽屉：默认收起，由左下角按钮拉开。
  const [drawerOpen, setDrawerOpen] = useState(false)
  const [view, setView] = useState<"cases" | "guide" | "history">("cases")
  const [page, setPage] = useState(1)
  // 隐藏案件单独分页：总页数与正常案件互不影响。
  const [hiddenPage, setHiddenPage] = useState(1)
  const [pageSize, setPageSize] = useState(10)
  const [data, setData] = useState<CasePage | null>(null)
  const [loading, setLoading] = useState(true)
  const [listError, setListError] = useState("")
  const [reloadKey, setReloadKey] = useState(0)
  const [query, setQuery] = useState("")
  // 案件号全局搜索；null 表示没有开启搜索。
  const [searchId, setSearchId] = useState<number | null>(null)
  // 被隐藏的案件（已认领/超期）默认不进列表，但用户可以一键展开查看。
  const [showHidden, setShowHidden] = useState(false)
  // 列表里的 createdAt 有时并不是举报时间（例如 #53638），只有详情才准确。
  // 详情确认超期后记在这里，否则这个案件会一直留在「可审理」里。
  const [staleIds, setStaleIds] = useState<Set<number>>(new Set())
  const [selectedId, setSelectedId] = useState<number | null>(null)
  const [mode, setMode] = useState<Mode>(canUseAi ? "ai" : "manual")
  const [toast, setToast] = useState<{ text: string; tone: Tone } | null>(null)
  const [auto, setAuto] = useState<AutoSettings>(loadAuto)
  const [autoStatus, setAutoStatus] = useState<AutoStatus>(DEFAULT_AUTO_STATUS)
  const [aiStatus, setAiStatus] = useState<AiStatus | null>(null)
  const [settingsOpen, setSettingsOpen] = useState(false)
  const [pendingConfirm, setPendingConfirm] = useState<number[]>(loadPending)
  const [stats, setStats] = useState<DayStats>(loadStats)
  const [history, setHistory] = useState<JudgeRecord[]>(loadHistory)
  // 服务端 records.json 里的审核记录；null = 还没取到 / 读取失败，用本地记录兜底。
  const [cloudRecords, setCloudRecords] = useState<RecordEntry[] | null>(null)
  // 正在查看详情的历史案件；null = 关闭。
  const [historyRecord, setHistoryRecord] = useState<RecordEntry | null>(null)
  // 正在手动获取满意度的案件号。
  const [satisfactionFetching, setSatisfactionFetching] = useState<Set<number>>(
    new Set(),
  )
  // 已审核案件列表同步下来的历史：按当前 token 隔离缓存，换 token 不会串。
  const [syncedHistory, setSyncedHistory] = useState<RecordEntry[]>(
    () => loadSyncedHistory(owner).records,
  )
  const [syncedAt, setSyncedAt] = useState(() => loadSyncedHistory(owner).at)
  const [syncingHistory, setSyncingHistory] = useState(false)
  // 审核记录只写服务端本地文件；历史另由遍历已审核案件列表、按审核员匹配得出。
  const historyEntries = useMemo<RecordEntry[]>(() => {
    const merged = new Map<number, RecordEntry>()
    for (const item of history) {
      merged.set(item.caseId, {
        caseId: item.caseId,
        judgeRecordId: item.judgeRecordId,
        nickname: item.nickname,
        content: item.content,
        action: (item.action ?? 1) as Action,
        remark: "",
        source: "manual",
        confidence: null,
        satisfaction: item.satisfaction,
        satisfactionFetched: item.satisfactionFetched,
        satisfactionAt: item.satisfactionTriedAt,
        caseReportTime: "",
        createdAt: item.at,
        tokenMasked: "",
        pending: true,
      })
    }
    for (const item of cloudRecords ?? []) {
      const local = merged.get(item.caseId)
      // 谁更新得更晚用谁的：刚手动取到的值不能被旧值盖掉（0 也是有效满意度）。
      const localNewer =
        (local?.satisfactionAt || 0) > (item.satisfactionAt || 0) &&
        Boolean(local?.satisfactionFetched)
      merged.set(item.caseId, {
        ...item,
        // 服务端字段优先；服务端没有的用本机兜底（0 是有效满意度，只有 null 才兜底）。
        remark: item.remark || local?.remark || "",
        satisfaction: localNewer
          ? (local?.satisfaction ?? item.satisfaction ?? null)
          : (item.satisfaction ?? local?.satisfaction ?? null),
        satisfactionFetched:
          item.satisfactionFetched || Boolean(local?.satisfactionFetched),
        satisfactionAt: Math.max(item.satisfactionAt || 0, local?.satisfactionAt || 0),
        caseReportTime: item.caseReportTime || local?.caseReportTime || "",
        tokenMasked: item.tokenMasked || local?.tokenMasked || "",
        judgeRecordId: item.judgeRecordId || local?.judgeRecordId || 0,
      })
    }
    // 已审核列表同步结果只补缺：服务端/本机已有记录优先（带真实处理结果和满意度）。
    for (const item of syncedHistory) {
      if (!merged.has(item.caseId)) merged.set(item.caseId, item)
    }
    return [...merged.values()]
      .sort((a, b) => b.createdAt - a.createdAt)
      .slice(0, HISTORY_LIMIT)
  }, [cloudRecords, history, syncedHistory])
  const historyCount = historyEntries.length
  // 详情弹窗里的记录跟着最新数据走，手动获取满意度后立即刷新。
  const historyDetailRecord = historyRecord
    ? (historyEntries.find((entry) => entry.caseId === historyRecord.caseId) ??
      historyRecord)
    : null
  // 服务器端 24 小时自动审核是否开启：开着时本机不再跑自己那套循环，
  // 否则两个循环会抢同一批案件，表现就是"有时候提交失败/不生效"。
  const [serverAutoOn, setServerAutoOn] = useState(false)
  const [autoBusy, setAutoBusy] = useState(false)
  const autoRunning = useRef(false)
  const handledAuto = useRef(new Set<number>())
  const caseCooldown = useRef(
    new Map<number, { until: number; attempts: number }>(),
  )
  // AI 建议按案件缓存：切走再切回来不再重新分析，避免白白消耗模型配额。
  const suggestionCache = useRef(new Map<number, Suggestion>())

  // 案件一旦被认领（checkerName 不再是空/--），它的 AI 分析就没有意义了，直接丢掉；
  // 待确认提醒也要一起消掉，否则案件处理完了横幅还一直挂在那里。
  const pruneClaimed = useCallback((list: CaseSummary[]) => {
    const claimed = new Set<number>()
    for (const item of list) {
      if (unclaimed(item.checkerName)) continue
      suggestionCache.current.delete(item.id)
      handledAuto.current.delete(item.id)
      claimed.add(item.id)
    }
    if (claimed.size === 0) return
    setPendingConfirm((current) => {
      const next = current.filter((id) => !claimed.has(id))
      return next.length === current.length ? current : next
    })
  }, [])
  const selectedRef = useRef<number | null>(null)
  selectedRef.current = selectedId
  const autoRef = useRef(auto)
  autoRef.current = auto
  const autoSelected = useRef(false)

  const notify = useCallback(
    (text: string, tone: Tone = "success") => setToast({ text, tone }),
    [],
  )
  const fail = useCallback(
    (error: unknown) =>
      isExpired(error)
        ? onLogout(EXPIRED_MESSAGE)
        : notify(messageOf(error), "error"),
    [notify, onLogout],
  )
  const refresh = useCallback(
    () =>
      setReloadKey(
        (key) =>
          key +
          1,
      ),
    [],
  )
  const saveAuto = useCallback((next: AutoSettings) => {
    setAuto(next)
    try {
      // enabled / autoSubmit 每次启动都会重置为开启，不写进 localStorage。
      const {
        enabled: _sessionOnly,
        autoSubmit: _autoSubmitSessionOnly,
        ...persisted
      } = next
      localStorage.setItem(AUTO_KEY, JSON.stringify(persisted))
    } catch {}
    // 偏好存在服务端的 prefs.json 里，按 token 归属，换浏览器也能恢复同一套。
    void api.savePrefs({ ...next }).catch(() => {})
  }, [api])

  // 进入界面后拉一次偏好，覆盖本地默认值（间隔、阈值、优先删除）。
  useEffect(() => {
    let cancelled = false
    api
      .prefs()
      .then(({ data }) => {
        if (cancelled || !data) return
        const stored = data as Partial<AutoSettings>
        const hasAny =
          typeof stored.interval === "number" ||
          typeof stored.threshold === "number" ||
          typeof stored.preferDelete === "boolean"
        if (!hasAny) return
        setAuto((current) => ({
          ...current,
          interval:
            typeof stored.interval === "number"
              ? clampInterval(stored.interval)
              : current.interval,
          threshold:
            typeof stored.threshold === "number" &&
            AUTO_THRESHOLDS.includes(stored.threshold)
              ? stored.threshold
              : current.threshold,
          preferDelete:
            typeof stored.preferDelete === "boolean"
              ? stored.preferDelete
              : current.preferDelete,
        }))
      })
      .catch((error) => console.warn("[prefs] 读取偏好失败：", messageOf(error)))
    return () => {
      cancelled = true
    }
  }, [api])

  // 读取服务器端自动审核开关：开启时本机循环让位，避免两边抢同一批案件。
  useEffect(() => {
    let cancelled = false
    // 首屏先让列表跑：非手动打开设置时延后再读，别和案件请求抢同一条连接。
    const idle = window.setTimeout(
      () => {
        api
          .serverAuto()
          .then((status) => !cancelled && setServerAutoOn(status.enabled))
          .catch(() => {})
      },
      settingsOpen ? 0 : 4000,
    )
    return () => {
      cancelled = true
      window.clearTimeout(idle)
    }
  }, [api, settingsOpen])

  useEffect(() => {
    if (!toast) return
    const timer = window.setTimeout(() => setToast(null), 3600)
    return () => window.clearTimeout(timer)
  }, [toast])

  // 页面版本轮询：后端重新打包后（index.html 指纹变化）自动刷新，
  // 彻底解决"新功能/新界面要手动重启才生效"的问题。
  useEffect(() => {
    let baseline: string | null = null
    let stopped = false
    const check = async () => {
      try {
        const result = await api.version()
        const build = String(result.build ?? "")
        if (!build || stopped) return
        if (baseline === null) baseline = build
        else if (baseline !== build) {
          baseline = build
          console.info("[version] 检测到新版本，自动刷新页面")
          window.location.reload()
        }
      } catch {}
    }
    // 版本轮询不参与首屏：先让列表和会话跑完，再起 20 秒一次的轮询。
    let timer = 0
    const idle = window.setTimeout(() => {
      void check()
      timer = window.setInterval(check, 20_000)
    }, 8000)
    return () => {
      stopped = true
      window.clearTimeout(idle)
      window.clearInterval(timer)
    }
  }, [api])

  useEffect(() => {
    try {
      localStorage.setItem(PENDING_KEY, JSON.stringify(pendingConfirm))
    } catch {}
  }, [pendingConfirm])

  // 今日统计：自动审核每次收尾、以及手动提交成功时累加。
  const bumpStats = useCallback((delta: Partial<Omit<DayStats, "date">>) => {
    setStats((current) => {
      const today = todayKey()
      const base = current.date === today ? current : emptyStats(today)
      const next: DayStats = {
        date: today,
        submitted: base.submitted + (delta.submitted ?? 0),
        waiting: base.waiting + (delta.waiting ?? 0),
        failed: base.failed + (delta.failed ?? 0),
        manual: base.manual + (delta.manual ?? 0),
      }
      try {
        localStorage.setItem(STATS_KEY, JSON.stringify(next))
      } catch {}
      return next
    })
  }, [])

  const updateHistory = useCallback(
    (updater: (list: JudgeRecord[]) => JudgeRecord[]) => {
      setHistory((current) => {
        const next = updater(current).slice(0, HISTORY_LIMIT)
        saveHistory(next)
        return next
      })
    },
    [],
  )

  /**
   * 拉取审核记录：服务端按 access-token 归属，同一个 token 的多端看到同一份。
   */
  const hasCommunityCredential = Boolean(session.community?.hasToken)
  const loadRecords = useCallback(async () => {
    if (!hasCommunityCredential) {
      setCloudRecords(null)
      return
    }
    try {
      const result = await api.records(200)
      setCloudRecords(result.records)
    } catch (error) {
      console.warn("[records] 审核记录读取失败：", messageOf(error))
      setCloudRecords(null)
    }
  }, [api, hasCommunityCredential])

  /**
   * 提交成功后登记一条历史：先本地入队，再查一次已判决列表，按案件号补 judgeRecordId。
   * 满意度要等 24 小时后才有意义，之后每天刷新一次。
   */
  const rememberJudged = useCallback(
    async (
      caseId: number,
      info: { action: Action | null; nickname: string; content: string },
    ) => {
      const entry: JudgeRecord = {
        caseId,
        judgeRecordId: 0,
        nickname: info.nickname,
        content: info.content,
        action: info.action,
        at: Date.now(),
        satisfaction: null,
        satisfactionFetched: false,
        satisfactionTriedAt: 0,
      }
      updateHistory((list) => [
        entry,
        ...list.filter((item) => item.caseId !== caseId),
      ])
      try {
        // 提交刚返回，等后端落库一下再查。
        await new Promise((resolve) => window.setTimeout(resolve, 1200))
        const page = await api.judged(1, 50)
        const judgeRecordId = judgeIdForCase(page, caseId)
        if (judgeRecordId <= 0) {
          console.warn(
            `[history] 案件 #${caseId} 尚未出现在判决列表，刷新满意度时会再按案件号匹配`,
          )
          return
        }
        updateHistory((list) =>
          list.map((item) =>
            item.caseId === caseId && item.judgeRecordId === 0
              ? { ...item, judgeRecordId }
              : item,
          ),
        )
        // 服务端记录也补上判决 ID，这样换一个 token 缓存第二天也能取满意度。
        await api.setJudgeRecordId(caseId, judgeRecordId).catch(() => {})
        void loadRecords()
      } catch (error) {
        console.warn("[history] 取判决记录失败：", messageOf(error))
      }
    },
    [api, updateHistory, loadRecords],
  )

  /** 防止多轮刷新重叠；同时避免列表刷新触发的重复请求。 */
  const satisfactionBusy = useRef(false)
  const satisfactionRanAt = useRef(0)

  /**
   * 刷新满意度：满 24 小时后每天取一次。
   * 判决记录号由服务端按案件号在「我已判决」列表里精确匹配，
   * 再用 myOpinion 的赞同/反对数算比例（上游返回的 ratio 不准）。
   */
  const refreshSatisfaction = useCallback(
    async (force = false) => {
      if (satisfactionBusy.current) return
      // 一分钟内重复触发（列表刷新、服务端回写都会触发）直接跳过。
      if (!force && Date.now() - satisfactionRanAt.current < 60_000) return
      const now = Date.now()
      const due = historyEntries.filter((item) => {
        // 「正常显示」的判决没有删除评论，不抓取满意度。
        if (Number(item.action) === 1) return false
        // 刚提交不到一天，满意度没有参考价值。
        if (now - item.createdAt < SATISFACTION_DELAY_MS) return false
        // 手动刷新：满一天的全部重取一遍。
        if (force) return true
        // 上次尝试（不管成功没成功）满 24 小时才重取。
        return now - (item.satisfactionAt || 0) >= SATISFACTION_REFRESH_MS
      })
      if (due.length === 0) return
      satisfactionRanAt.current = now
      satisfactionBusy.current = true
      let touched = false
      const batches: SatisfactionItem[][] = []
      for (let start = 0; start < due.length; start += SATISFACTION_BATCH) {
        batches.push(
          due
            .slice(start, start + SATISFACTION_BATCH)
            .map((item) => ({
              caseId: item.caseId,
              judgeRecordId: item.judgeRecordId,
            })),
        )
      }
      try {
        // 一批等一批串行发是历史页最慢的一段；改成几路并发，取到一批就回写一批。
        await mapWithConcurrency(batches, SATISFACTION_CONCURRENCY, async (batch) => {
          try {
            const result = await api.refreshSatisfaction(batch)
            if (result.items.length === 0) return
            touched = true
            const map = new Map(result.items.map((item) => [item.caseId, item]))
            const checkedAt = Date.now()
            updateHistory((list) =>
              list.map((row) => {
                const item = map.get(row.caseId)
                if (!item) return row
                return {
                  ...row,
                  judgeRecordId:
                    item.judgeRecordId > 0 ? item.judgeRecordId : row.judgeRecordId,
                  satisfaction: item.fetched ? item.satisfaction : row.satisfaction,
                  satisfactionFetched: item.fetched || row.satisfactionFetched,
                  // 这次已经尝试过：成功记新值，暂时取不到也记时间，同一天不反复请求。
                  satisfactionTriedAt: checkedAt,
                }
              }),
            )
          } catch (error) {
            // 单批失败不影响其它批：这里吞掉，明天这一条还会再进 due。
            console.warn("[history] 取满意度失败：", messageOf(error))
          }
        })
      } finally {
        satisfactionBusy.current = false
        if (touched) void loadRecords()
      }
    },
    [api, historyEntries, updateHistory, loadRecords],
  )

  /** 历史记录里单条手动获取满意度：不受“每天一次”限制，用户点一次就查一次。 */
  const fetchSatisfaction = useCallback(
    async (record: RecordEntry) => {
      if (Number(record.action) === 1) {
        notify("正常显示的案件不统计满意度", "error")
        return
      }
      if (Date.now() - record.createdAt < SATISFACTION_DELAY_MS) {
        notify("提交满 24 小时后才能获取满意度", "error")
        return
      }
      if (satisfactionFetching.has(record.caseId)) return
      setSatisfactionFetching((current) => new Set(current).add(record.caseId))
      try {
        const result = await api.refreshSatisfaction([
          { caseId: record.caseId, judgeRecordId: record.judgeRecordId },
        ])
        const item = result.items.find((entry) => entry.caseId === record.caseId)
        if (!item) {
          notify(`案件 #${record.caseId} 暂时取不到满意度，稍后再试`, "error")
          return
        }
        const checkedAt = Date.now()
        updateHistory((list) =>
          list.map((row) =>
            row.caseId === record.caseId
              ? {
                  ...row,
                  judgeRecordId:
                    item.judgeRecordId > 0
                      ? item.judgeRecordId
                      : row.judgeRecordId,
                  satisfaction: item.fetched
                    ? item.satisfaction
                    : row.satisfaction,
                  satisfactionFetched:
                    item.fetched || row.satisfactionFetched,
                  satisfactionTriedAt: checkedAt,
                }
              : row,
          ),
        )
        // 同一条也可能来自已审核列表同步，把满意度一起写进同步缓存。
        setSyncedHistory((list) => {
          if (!list.some((row) => row.caseId === record.caseId)) return list
          const next = list.map((row) =>
            row.caseId === record.caseId
              ? {
                  ...row,
                  judgeRecordId:
                    item.judgeRecordId > 0
                      ? item.judgeRecordId
                      : row.judgeRecordId,
                  satisfaction: item.fetched
                    ? item.satisfaction
                    : row.satisfaction,
                  satisfactionFetched:
                    item.fetched || row.satisfactionFetched,
                  satisfactionAt: checkedAt,
                }
              : row,
          )
          saveSyncedHistory(owner, syncedAt || Date.now(), next)
          return next
        })
        if (!item.fetched) {
          notify(`案件 #${record.caseId} 暂时取不到满意度，稍后再试`, "error")
        } else if (item.satisfaction === null) {
          notify(`案件 #${record.caseId} 暂无其他用户评价`)
        } else {
          notify(
            `案件 #${record.caseId} 满意度 ${Math.round(item.satisfaction * 100)}%`,
          )
        }
        void loadRecords()
      } catch (error) {
        notify(`获取满意度失败：${messageOf(error)}`, "error")
      } finally {
        setSatisfactionFetching((current) => {
          const next = new Set(current)
          next.delete(record.caseId)
          return next
        })
      }
    },
    [api, loadRecords, notify, owner, satisfactionFetching, syncedAt, updateHistory],
  )

  /** 从「已审核案件列表」同步当前审核员的历史；10 分钟内复用缓存。 */
  const syncedTryAt = useRef(0)
  const syncHistoryFromCases = useCallback(
    async (force = false) => {
      if (syncingHistory) return
      if (!force && Date.now() - syncedAt < SYNCED_HISTORY_TTL) return
      // 自动同步失败后一分钟内不重复请求，避免接口报错时反复刷新。
      if (!force && Date.now() - syncedTryAt.current < 60_000) return
      syncedTryAt.current = Date.now()
      setSyncingHistory(true)
      try {
        const result = await api.syncedHistory({
          limit: HISTORY_LIMIT,
          pages: 40,
          reviewerId: reviewerId > 0 ? reviewerId : undefined,
        })
        // 首次没有填审核员 ID 时，服务端反查出来的 ID 自动存进设置。
        if (reviewerId === 0 && result.reviewerId > 0) {
          onReviewerIdChange(result.reviewerId)
        }
        // 同步结果不包含满意度：同案件号沿用上次手动获取的满意度。
        const previous = new Map(
          syncedHistory.map((item) => [item.caseId, item]),
        )
        const entries = result.records.map((item) => {
          const mapped = mapSyncedEntry(item)
          const old = previous.get(mapped.caseId)
          return old
            ? {
                ...mapped,
                judgeRecordId: old.judgeRecordId,
                satisfaction: old.satisfaction,
                satisfactionFetched: old.satisfactionFetched,
                satisfactionAt: old.satisfactionAt,
              }
            : mapped
        })
        const at = Date.now()
        setSyncedHistory(entries)
        setSyncedAt(at)
        saveSyncedHistory(owner, at, entries)
        if (force) {
          notify(
            entries.length > 0
              ? `已从案件列表同步 ${entries.length} 条历史`
              : "最近案件里没有找到你的审核记录",
            entries.length > 0 ? "success" : "error",
          )
        }
      } catch (error) {
        if (force) notify(`历史同步失败：${messageOf(error)}`, "error")
        else console.warn("[history] 已审核列表同步失败：", messageOf(error))
      } finally {
        setSyncingHistory(false)
      }
    },
    [
      api,
      notify,
      onReviewerIdChange,
      reviewerId,
      owner,
      syncedAt,
      syncedHistory,
      syncingHistory,
    ],
  )

  useEffect(() => {
    if (!active) return
    void loadRecords()
  }, [active, loadRecords, view, reloadKey])

  // 记录加载完 / 切到历史页时刷新一次（内部按“每天一次”控制频率）。
  useEffect(() => {
    if (!active) return
    void refreshSatisfaction()
  }, [active, view, refreshSatisfaction])

  // 进入历史页时从已审核案件列表补一次历史（有 10 分钟缓存）。
  useEffect(() => {
    if (active && view === "history") void syncHistoryFromCases()
  }, [active, view, syncHistoryFromCases])

  // 页面长期开着时也要每天补一次，不能只靠切页触发。
  const satisfactionTick = useRef(refreshSatisfaction)
  useEffect(() => {
    satisfactionTick.current = refreshSatisfaction
  }, [refreshSatisfaction])
  useEffect(() => {
    if (!active) return
    const timer = window.setInterval(
      () => void satisfactionTick.current(),
      30 * 60 * 1000,
    )
    return () => window.clearInterval(timer)
  }, [active])

  // 已经取过的分页组合：切模式再切回来时复用内存里的列表，不重复扫全表。
  const loadedKey = useRef("")
  const listController = useRef<AbortController | null>(null)

  useEffect(() => {
    // 切到作品模式时不再拉评论列表：这一侧的子树还挂着，数据留着等用户切回来。
    if (!active) return
    const key = `${page}|${pageSize}|${hiddenPage}|${searchId ?? ""}|${reloadKey}`
    if (loadedKey.current === key) return
    loadedKey.current = key
    // 只有翻页 / 搜索 / 提交后才会取代上一次请求；切模式不打断在飞的请求。
    listController.current?.abort()
    const controller = new AbortController()
    listController.current = controller
    setLoading(true)
    setListError("")
    api
      .cases(page, pageSize, {
        reportId: searchId ?? undefined,
        hiddenPage,
        signal: controller.signal,
      })
      .then((result) => {
        if (controller.signal.aborted) return
        setData(result)
        pruneClaimed([...result.list, ...(result.hiddenList ?? [])])
        // 搜索命中或被隐藏的案件时自动展开，避免"搜到了却看不到"。
        if (
          result.search &&
          (result.hiddenList?.length ?? 0) > 0 &&
          result.list.length === 0
        ) {
          setShowHidden(true)
        }
        const wide = window.matchMedia("(min-width: 821px)").matches
        // 默认只自动选中可审案件；搜索命中隐藏案件时才自动选中它，避免一打开就展示超期案件。
        const first =
          result.list[0] ?? (result.search ? result.hiddenList?.[0] : undefined)
        if (!autoSelected.current && wide && first) setSelectedId(first.id)
        autoSelected.current = true
      })
      .catch((error) => {
        if (controller.signal.aborted) return
        // 这次没拿到数据，允许同样的分页组合重试。
        loadedKey.current = ""
        if (isExpired(error)) onLogout(EXPIRED_MESSAGE)
        else setListError(messageOf(error))
      })
      .finally(
        () =>
          !controller.signal.aborted &&
          setLoading(false),
      )
  }, [
    active,
    api,
    page,
    pageSize,
    hiddenPage,
    searchId,
    reloadKey,
    onLogout,
    pruneClaimed,
  ])
  // 退出登录卸载子树时才掐断在飞的列表请求；切模式不中断。
  useEffect(
    () => () => {
      listController.current?.abort()
    },
    [],
  )
  useEffect(() => {
    if (!active) return
    let cancelled = false
    api
      .aiStatus()
      .then(
        (status) =>
          !cancelled &&
          setAiStatus(status),
      )
      .catch(() => {})
    return () => {
      cancelled = true
    }
  }, [active, api, settingsOpen])
  useEffect(() => {
    if (!auto.enabled || !canUseAi || serverAutoOn) {
      setAutoStatus((status) => ({
        ...status,
        state: "idle",
        message: serverAutoOn
          ? "服务器端 24 小时审核运行中，本机循环已暂停"
          : auto.enabled
            ? "后端未配置 AI 服务，自动审核无法运行"
            : "自动审核已关闭",
      }))
      return
    }
    let stopped = false

    // 一个周期的独立状态：周期作废后，它的后续回调不会再提交任何东西。
    type Tick = { expired: boolean; scan: CaseSummary[] | null }

    const aborted = (tick: Tick) => stopped || tick.expired
    const coolingUntil = (id: number) => caseCooldown.current.get(id)?.until ?? 0

    function markFailure(id: number, message: string) {
      const attempts = (caseCooldown.current.get(id)?.attempts ?? 0) + 1
      // 1、2、4、8、16 分钟退避；用尽后交给人工，不再反复撞同一件案子。
      const backoff = Math.min(60_000 * 2 ** (attempts - 1), 16 * 60_000)
      caseCooldown.current.set(id, {
        until:
          attempts >= AUTO_MAX_ATTEMPTS
            ? Number.POSITIVE_INFINITY
            : Date.now() + backoff,
        attempts,
      })
      console.warn(`[auto] #${id} 第 ${attempts} 次失败：${message}`)
      return attempts
    }

    // 每个周期只扫一次列表：旧实现每处理一件案子就要重扫三页。
    async function scanCases(tick: Tick): Promise<CaseSummary[]> {
      if (tick.scan) return tick.scan
      const collected: CaseSummary[] = []
      for (let pageIndex = 1; pageIndex <= 3; pageIndex += 1) {
        const pageResult = await api.cases(pageIndex, 50, { raw: true })
        if (aborted(tick)) break
        collected.push(...pageResult.list)
        if (pageResult.list.length < 50) break
      }
      tick.scan = collected
      pruneClaimed(collected)
      return collected
    }

    async function findTarget(tick: Tick, skip: Set<number>) {
      const list = await scanCases(tick)
      const now = Date.now()
      return list.find(
        (item) =>
          unclaimed(item.checkerName) &&
          !handledAuto.current.has(item.id) &&
          !skip.has(item.id) &&
          item.id !== selectedRef.current &&
          coolingUntil(item.id) <= now &&
          // 超期或举报时间无法确认的案件不进入自动审核。
          actionable(item),
      )
    }

    async function submit(id: number, action: Action, suggestion: Suggestion) {
      const input = {
        action,
        remark: suggestion.remark,
        userMessage: action === 3 ? TEMPLATES[3].user : suggestion.userMessage,
        reporterMessage: suggestion.reporterMessage,
      }
      try {
        await api.verify(id, input)
      } catch (error) {
        // 社区接口对 action=2 偶发参数错误，用删除兜底重试一次。
        if (action !== 2) throw error
        console.warn(`[auto] #${id} 替换失败，改用删除重试：${messageOf(error)}`)
        await api.verify(id, {
          ...input,
          action: 3,
          userMessage: TEMPLATES[3].user,
        })
      }
    }

    type StepResult =
      | "submitted"
      | "waiting"
      | "skipped"
      | "failed"
      | "stale"
      | "idle"

    async function processOne(
      tick: Tick,
      skip: Set<number>,
    ): Promise<StepResult> {
      const target = await findTarget(tick, skip)
      if (!target || aborted(tick)) return "idle"

      const settings = autoRef.current
      try {
        const detail = await api.detail(target.id)
        if (aborted(tick)) return "skipped"
        // 超期或举报时间无法确认的案件不再分析/提交：列表时间不可靠，以详情为准。
        const age = caseAgeOf(detail)
        if (age === null || isStale(age)) {
          handledAuto.current.add(target.id)
          console.info(
            `[auto] #${target.id} ${age === null ? "举报时间未知" : "超过一周"}，跳过`,
          )
          return "stale"
        }
        // 列表已表明这个案件未认领；详情里的 checkerId 口径未必一致，
        // 只做短期避让，绝不据此永久跳过（旧逻辑会因此把所有案件判成"已认领"）。
        if (detail.checkerId !== 0) {
          console.info(
            `[auto] #${target.id} 详情显示已被认领（checkerId=${detail.checkerId}），5 分钟后再看`,
          )
          caseCooldown.current.set(target.id, {
            until: Date.now() + 5 * 60_000,
            attempts: 0,
          })
          return "skipped"
        }

        const suggestion = await api.suggest(target.id)
        if (aborted(tick)) return "skipped"
        console.info(
          `[auto] #${target.id} action=${suggestion.action} confidence=${suggestion.confidence}`,
        )

        // 优先删除：AI 判为替换时按删除提交，绕开上游对 action=2 的参数错误。
        const action: Action =
          settings.preferDelete && suggestion.action === 2
            ? 3
            : suggestion.action
        if (!settings.autoSubmit || suggestion.confidence < settings.threshold) {
          handledAuto.current.add(target.id)
          setPendingConfirm((list) =>
            list.includes(target.id) ? list : [...list, target.id],
          )
          notify(
            `AI 置信度 ${Math.round(suggestion.confidence * 100)}%，案件 #${target.id} 已转入待确认`,
            "error",
          )
          return "waiting"
        }

        await submit(target.id, action, suggestion)
        if (aborted(tick)) return "skipped"
        handledAuto.current.add(target.id)
        suggestionCache.current.set(target.id, suggestion)
        caseCooldown.current.delete(target.id)
        void rememberJudged(target.id, {
          action,
          nickname: target.nickname,
          content: target.content,
        })
        setPendingConfirm((list) => list.filter((id) => id !== target.id))
        notify(`已自动${actionLabel(action)} · 案件 #${target.id}`)
        return "submitted"
      } catch (error) {
        skip.add(target.id)
        if (isExpired(error)) {
          onLogout(EXPIRED_MESSAGE)
          return "skipped"
        }
        const attempts = markFailure(target.id, messageOf(error))
        if (!stopped) {
          notify(
            `案件 #${target.id} 自动处理失败（${attempts}/${AUTO_MAX_ATTEMPTS}）：${messageOf(error)}`,
            "error",
          )
        }
        return "failed"
      }
    }

    async function runTick() {
      if (stopped || autoRunning.current) return
      autoRunning.current = true
      const tick: Tick = { expired: false, scan: null }
      setAutoBusy(true)
      setAutoStatus((status) => ({
        ...status,
        state: "running",
        message: "正在检查新案件",
      }))
      const deadline = Date.now() + AUTO_TICK_LIMIT
      const skip = new Set<number>()
      const tally = { submitted: 0, waiting: 0, failed: 0, stale: 0 }
      let candidates = 0
      let unclaimedTotal = 0
      let staleTotal = 0

      const worker = (async () => {
        // 先看一眼候选量：没有未认领案件时，界面能直接说明原因。
        const all = await scanCases(tick)
        if (aborted(tick)) return
        const claimable = all.filter((item) => unclaimed(item.checkerName))
        unclaimedTotal = claimable.length
        staleTotal = claimable.filter((item) => !actionable(item)).length
        candidates = claimable.filter(
          (item) => !handledAuto.current.has(item.id),
        ).length
        for (let index = 0; index < AUTO_PER_TICK; index += 1) {
          if (aborted(tick)) return
          if (Date.now() > deadline) {
            tick.expired = true
            return
          }
          const result = await processOne(tick, skip)
          if (aborted(tick)) return
          if (result === "idle") return
          if (result === "submitted") tally.submitted += 1
          if (result === "waiting") tally.waiting += 1
          if (result === "failed") tally.failed += 1
          if (result === "stale") tally.stale += 1
        }
      })()

      try {
        // 兜底：即使某个请求卡住，也在这里把锁放掉，让下一轮能继续跑。
        await Promise.race([
          worker,
          new Promise((resolve) =>
            window.setTimeout(resolve, AUTO_TICK_LIMIT + 5_000),
          ),
        ])
        if (Date.now() > deadline) tick.expired = true
      } catch (error) {
        if (!stopped) fail(error)
      } finally {
        const expired = tick.expired
        // 锁放掉后这个周期即作废，避免它和新周期同时提交。
        tick.expired = true
        autoRunning.current = false
        if (tally.submitted || tally.waiting || tally.failed) {
          bumpStats({
            submitted: tally.submitted,
            waiting: tally.waiting,
            failed: tally.failed,
          })
        }
        if (!stopped) {
          const now = Date.now()
          const cooling = Array.from(caseCooldown.current.values()).filter(
            (entry) => entry.until > now,
          ).length
          setAutoBusy(false)
          setAutoStatus((status) => ({
            state: expired || tally.failed > 0 ? "error" : "waiting",
            message: autoMessage(
              tally.submitted,
              tally.waiting,
              tally.failed,
              expired,
              autoRef.current.autoSubmit,
              unclaimedTotal,
              staleTotal,
            ),
            lastRunAt: now,
            submitted: status.submitted + tally.submitted,
            waiting: status.waiting + tally.waiting,
            failed: status.failed + tally.failed,
            cooling,
            candidates,
            stale: staleTotal,
          }))
        }
        if (tally.submitted > 0 && !stopped) refresh()
        if (!stopped) {
          // 刷新密钥健康状态，设置面板里能看到当前用的是哪一把。
          api
            .aiStatus()
            .then((status) => !stopped && setAiStatus(status))
            .catch(() => {})
        }
      }
    }

    void runTick()
    const timer = window.setInterval(runTick, Math.max(10, auto.interval) * 1000)
    return () => {
      stopped = true
      window.clearInterval(timer)
    }
  }, [
    auto.enabled,
    auto.interval,
    api,
    canUseAi,
    serverAutoOn,
    bumpStats,
    fail,
    notify,
    onLogout,
    pruneClaimed,
    refresh,
    rememberJudged,
  ])

  // 服务端已经把列表分成「正常」和「隐藏」两组，各自返回分页数据。
  const normalCases = useMemo(() => data?.list ?? [], [data])
  const hiddenServerCases = useMemo(() => data?.hiddenList ?? [], [data])
  const allCases = useMemo(() => {
    const seen = new Map<number, CaseSummary>()
    for (const item of [...normalCases, ...hiddenServerCases])
      seen.set(item.id, item)
    return [...seen.values()]
  }, [normalCases, hiddenServerCases])
  // 详情页的只读状态以列表为准；列表里没有该案件（如按案件号搜索）时为 null，退回详情字段。
  const selectedClaimed = useMemo(() => {
    if (selectedId === null) return null
    const summary = allCases.find((item) => item.id === selectedId)
    return summary ? !unclaimed(summary.checkerName) : null
  }, [allCases, selectedId])
  const matchesQuery = useCallback(
    (item: CaseSummary, keyword: string) =>
      !keyword ||
      [
        item.nickname,
        item.content,
        String(item.id),
        item.userId,
        item.checkerName,
      ].some((value) => value.toLowerCase().includes(keyword)),
    [],
  )
  const markStale = useCallback(
    (id: number) =>
      setStaleIds((prev) => {
        if (prev.has(id)) return prev
        const next = new Set(prev)
        next.add(id)
        return next
      }),
    [],
  )
  // 案件号搜索时不再做本地文字过滤，直接展示服务端结果。
  const keyword = searchId !== null ? "" : query.trim().toLowerCase()
  // 详情确认超期、列表时间超期或时间无法确认的案件，都先在本地移入隐藏组。
  const movedStale = useMemo(
    () =>
      normalCases.filter(
        (item) =>
          staleIds.has(item.id) ||
          timeUnknown(item.createdAt) ||
          isStale(parseWhen(item.createdAt)),
      ),
    [normalCases, staleIds],
  )
  const filtered = useMemo(
    () =>
      normalCases.filter(
        (item) =>
          !staleIds.has(item.id) &&
          !timeUnknown(item.createdAt) &&
          !isStale(parseWhen(item.createdAt)) &&
          matchesQuery(item, keyword),
      ),
    [normalCases, staleIds, matchesQuery, keyword],
  )
  // 隐藏案件（已认领或超期）单独成组，展开后按自己的页码显示。
  const hiddenCases = useMemo(() => {
    const seen = new Set<number>()
    return [...hiddenServerCases, ...movedStale].filter((item) => {
      if (seen.has(item.id)) return false
      seen.add(item.id)
      return matchesQuery(item, keyword)
    })
  }, [hiddenServerCases, movedStale, matchesQuery, keyword])
  // 隐藏案件的已认领 / 超期统计：以服务端全量分类为准。
  const hiddenStats = useMemo(
    () => ({
      claimed: Math.max(
        data?.hiddenClaimedTotal ?? 0,
        hiddenServerCases.filter((item) => !unclaimed(item.checkerName)).length,
      ),
      stale:
        Math.max(
          data?.hiddenStaleTotal ?? 0,
          hiddenServerCases.filter(
            (item) =>
              timeUnknown(item.createdAt) ||
              isStale(parseWhen(item.createdAt)),
          ).length,
        ) + movedStale.length,
    }),
    [data, hiddenServerCases, movedStale],
  )
  // 服务端按列表时间分类；详情确认超期的案件由本地补进隐藏组。
  const hiddenTotal = (data?.hiddenTotal ?? 0) + movedStale.length
  const normalTotal = data?.normalTotal ?? data?.total ?? 0
  // 正常与隐藏两组各自计算总页数，互不影响。
  const totalPages = data?.search
    ? 1
    : Math.max(1, Math.ceil(normalTotal / pageSize))
  const hiddenTotalPages = data?.search
    ? 1
    : Math.max(1, Math.ceil(hiddenTotal / pageSize))
  // 删除/提交后列表变短时，把页码收回到有效范围。
  useEffect(() => {
    if (data && page > totalPages) setPage(totalPages)
  }, [data, page, totalPages])
  useEffect(() => {
    if (data && hiddenPage > hiddenTotalPages)
      setHiddenPage(hiddenTotalPages)
  }, [data, hiddenPage, hiddenTotalPages])

  // 纯数字按案件号全局搜索（走上游 reportId），其它关键词只过滤当前页。
  function runSearch() {
    const text = query.trim()
    if (!/^\d{1,18}$/.test(text) || Number(text) <= 0) return
    setSearchId(Number(text))
    setSelectedId(null)
    autoSelected.current = false
    setPage(1)
    setHiddenPage(1)
  }

  function clearSearch() {
    setQuery("")
    setSearchId(null)
    setPage(1)
    setHiddenPage(1)
  }
  const unread = Object.values(
    session.unread?.messageUnread ??
      {},
  ).reduce(
    (sum, count) =>
      sum +
      (Number(count) ||
        0),
    0,
  )

  // 历史审核列表在顶部统一合并，这里只保留 token 是否失效的判断。
  const tokenExpired = !demo && !session.community?.hasToken

  function handleSubmitted(
    id: number,
    info: { action: Action; nickname: string; content: string },
  ) {
    bumpStats({ manual: 1 })
    void rememberJudged(id, info)
    setPendingConfirm((list) =>
      list.filter(
        (item) =>
          item !==
          id,
      ),
    )
    const index = allCases.findIndex((item) => item.id === id)
    const remaining = allCases.filter((item) => item.id !== id)
    setSelectedId(
      remaining[
        Math.min(
          Math.max(index, 0),
          remaining.length -
            1,
        )
      ]?.id ??
        null,
    )
    notify(
      demo
        ? "示例提交成功，未发送到社区"
        : "审核结果已提交，案件已归属当前审核员",
    )
    refresh()
  }

  /** 删除一条历史记录：服务端记录与本机缓存一起删掉。 */
  async function removeRecord(record: RecordEntry) {
    if (
      !window.confirm(
        `删除案件 #${record.caseId} 的历史记录？服务端记录会一起删除，且无法恢复。`,
      )
    )
      return
    try {
      await api.deleteRecord(record.caseId)
      updateHistory((list) => list.filter((item) => item.caseId !== record.caseId))
      setCloudRecords((list) =>
        list ? list.filter((item) => item.caseId !== record.caseId) : list,
      )
      setHistoryRecord((current) =>
        current?.caseId === record.caseId ? null : current,
      )
      notify("历史记录已删除")
      void loadRecords()
    } catch (error) {
      fail(error)
    }
  }

  // 提交失败（例如案件已被他人处理、上游 422）时把该案件移出待确认队列，
  // 否则「AI 不确定，需人工确认」的横幅会一直提醒同一件案子。
  function handleSubmitFailed(id: number, error: unknown) {
    fail(error)
    setPendingConfirm((list) => {
      const next = list.filter((item) => item !== id)
      return next.length === list.length ? list : next
    })
    refresh()
  }

  return (
    <div
      className={`console ${
        selectedId !==
        null
          ? "has-selection"
          : ""
      }${drawerOpen ? " drawer-open" : ""}`}
    >
      <div
        className="sidebar-scrim"
        aria-hidden="true"
        onClick={() => setDrawerOpen(false)}
      />
      <button
        type="button"
        className="sidebar-toggle"
        aria-label="展开侧边栏"
        aria-expanded={drawerOpen}
        onClick={() => setDrawerOpen(true)}
      >
        <Icon name="menu" size={20} />
      </button>
      <aside className="sidebar">
        <div className="sidebar-brand">
          <AppMark />
          <div>
            <strong>风纪审核</strong>
            <span>社区内容安全</span>
          </div>
        </div>
        <nav
          className="sidebar-nav"
          aria-label="主导航"
          onClick={() => setDrawerOpen(false)}
        >
          <p className="sidebar-caption">审核</p>
          <button
            className={`side-item ${
              view ===
              "cases"
                ? "active"
                : ""
            }`}
            onClick={() => setView("cases")}
          >
            <Icon name="tray" size={17} />
            <span className="side-label">待审核</span>
            {data && <span className="side-count">{normalTotal}</span>}
          </button>
          <button
            className={`side-item ${
              view ===
              "history"
                ? "active"
                : ""
            }`}
            onClick={() => setView("history")}
          >
            <Icon name="clock" size={17} />
            <span className="side-label">历史审核</span>
            {historyCount > 0 && (
              <span className="side-count">{historyCount}</span>
            )}
          </button>
          <button
            className={`side-item ${
              view ===
              "guide"
                ? "active"
                : ""
            }`}
            onClick={() => setView("guide")}
          >
            <Icon name="book" size={17} />
            <span className="side-label">审核规范</span>
          </button>
          <p className="sidebar-caption">自动化</p>
          <button
            className={`side-item ${auto.enabled ? "active" : ""}`}
            onClick={() => setSettingsOpen(true)}
          >
            <Icon name="bolt" size={17} />
            <span className="side-label">自动审核</span>
            {auto.enabled && (
              <span className="side-dot" aria-label="自动审核运行中" />
            )}
          </button>
          <button className="side-item" onClick={() => setSettingsOpen(true)}>
            <Icon name="gear" size={17} />
            <span className="side-label">审核设置</span>
          </button>
        </nav>
        <ModeSwitch
          mode={active ? "comment" : "composition"}
          onSwitch={(next) => {
            setDrawerOpen(false)
            onModeChange(next)
          }}
        />
        <div className="sidebar-account">
          <Avatar name="审" size="sm" />
          <div>
            <strong>{demo ? "示例数据" : "社区审核员"}</strong>
            <span
              title={
                session.community?.masked
                  ? `凭证 ${session.community.masked}`
                  : undefined
              }
            >
              {demo
                ? "示例数据，不会提交"
                : tokenExpired
                  ? "尚未获取社区凭证"
                  : session.community?.masked
                    ? `凭证 ${session.community.masked}`
                    : unread
                      ? `${unread} 条未读消息`
                      : "访客模式"}
            </span>
          </div>
          <button
            className="icon-button"
            aria-label="退出登录"
            title="退出登录"
            onClick={() => onLogout()}
          >
            <Icon name="logout" size={17} />
          </button>
        </div>
      </aside>

      {view ===
      "guide" ? (
        <Guidelines />
      ) : view === "history" ? (
        <HistoryPane
          records={historyEntries}
          stats={stats}
          fetching={satisfactionFetching}
          syncing={syncingHistory}
          syncedAt={syncedAt}
          onRefresh={() => void refreshSatisfaction(true)}
          onSyncHistory={() => void syncHistoryFromCases(true)}
          onFetchSatisfaction={(record) => void fetchSatisfaction(record)}
          onClear={() => {
            updateHistory(() => [])
            void loadRecords()
          }}
          onOpen={setHistoryRecord}
          onDelete={(record) => void removeRecord(record)}
        />
      ) : (
        <>
          <section className="list-pane" aria-label="待审核案件">
            {loading && data && <span className="refresh-bar" aria-hidden="true" />}
            <header className="list-header">
              <div className="list-title">
                <h1>待审核</h1>
                <span>
                  {data
                    ? searchId !== null
                      ? `案件 #${searchId} 搜索结果`
                      : `${normalTotal} 个待审核案件`
                    : "正在加载"}
                  {auto.enabled && canUseAi
                    ? ` · 自动审核${autoBusy ? "执行中" : "待命"}`
                    : ""}
                  {hiddenTotal > 0
                    ? ` · 已隐藏 ${[
                        hiddenStats.claimed ? `${hiddenStats.claimed} 件已认领` : "",
                        hiddenStats.stale ? `${hiddenStats.stale} 件超期` : "",
                      ]
                        .filter(Boolean)
                        .join("、")}`
                    : ""}
                </span>
              </div>
              <div className="list-actions">
                <button
                  className="icon-button"
                  aria-label="刷新"
                  title="刷新"
                  onClick={refresh}
                  disabled={loading}
                >
                  <Icon name="refresh" size={17} />
                </button>
                <button
                  className="icon-button mobile-only"
                  aria-label="退出登录"
                  onClick={() => onLogout()}
                >
                  <Icon name="logout" size={17} />
                </button>
              </div>
            </header>
            <div className="stats-bar" role="group" aria-label="今日统计">
              <div className="stat-cell stat-blue">
                <Icon name="bolt" size={14} />
                <strong>{stats.submitted}</strong>
                <span>自动处理</span>
              </div>
              <div className="stat-cell stat-orange">
                <Icon name="alert" size={14} />
                <strong>{stats.waiting}</strong>
                <span>转人工</span>
              </div>
              <div className="stat-cell stat-green">
                <Icon name="check" size={14} />
                <strong>{stats.manual}</strong>
                <span>手动提交</span>
              </div>
              <div className="stat-cell">
                <Icon name="clock" size={14} />
                <strong>{historyCount}</strong>
                <span>历史记录</span>
              </div>
            </div>
            {auto.enabled && canUseAi && (
              <div
                className={`auto-status auto-status-${autoStatus.state}`}
                role="status"
              >
                <Icon
                  name={
                    autoStatus.state ===
                    "error"
                      ? "alert"
                      : "bolt"
                  }
                  size={14}
                />
                <span>
                  {autoStatus.state ===
                  "running"
                    ? "正在检查新案件…"
                    : autoStatus.message}
                  {autoStatus.lastRunAt &&
                  autoStatus.state !==
                    "running"
                    ? ` · 上次检查 ${clockOf(autoStatus.lastRunAt)}`
                    : ""}
                  {autoStatus.cooling >
                  0
                    ? ` · ${autoStatus.cooling} 件退避中`
                    : ""}
                </span>
                {autoStatus.submitted >
                  0 && (
                  <span className="auto-status-total">
                    已自动处理 {autoStatus.submitted} 件
                  </span>
                )}
              </div>
            )}
            {pendingConfirm.length >
              0 && (
              <div className="auto-banner" role="status">
                <Icon name="alert" size={15} />
                <span>
                  {pendingConfirm.length} 个案件 AI 不确定，需人工确认
                </span>
                <button
                  type="button"
                  className="btn btn-sm"
                  onClick={() => setSelectedId(pendingConfirm[0])}
                >
                  去确认
                </button>
                <button
                  type="button"
                  className="btn btn-sm btn-secondary"
                  onClick={() => setPendingConfirm([])}
                >
                  忽略
                </button>
              </div>
            )}
            {tokenExpired && (
              <div className="auto-banner" role="status">
                <Icon name="key" size={15} />
                <span>
                  {demo
                    ? "示例模式"
                    : "还没有可用的社区凭证（access-token），审核与自动提交无法进行；请到设置里重新粘贴"}
                </span>
                {!demo && (
                  <button
                    type="button"
                    className="btn btn-sm"
                    onClick={() => setSettingsOpen(true)}
                  >
                    去设置
                  </button>
                )}
              </div>
            )}
            <form
              className="search-field"
              onSubmit={(event) => {
                event.preventDefault()
                runSearch()
              }}
            >
              <Icon name="search" size={15} />
              <input
                type="search"
                aria-label="搜索案件号或当前页内容"
                placeholder="案件号 / 当前页关键词"
                value={query}
                onChange={(event) => {
                  setQuery(event.target.value)
                  // 改动关键词即退出案件号搜索，避免结果和输入不一致。
                  if (searchId !== null) setSearchId(null)
                }}
              />
              {(query || searchId !== null) && (
                <button
                  type="button"
                  className="search-clear"
                  aria-label="清除搜索"
                  onClick={clearSearch}
                >
                  <Icon name="close" size={13} />
                </button>
              )}
            </form>
            {searchId !== null && (
              <div className="search-result-bar" role="status">
                <span>
                  案件 #{searchId}：共{" "}
                  {(data?.list.length ?? 0) + (data?.hiddenList?.length ?? 0)}{" "}
                  条记录
                </span>
                <button
                  type="button"
                  className="link-pill"
                  onClick={clearSearch}
                >
                  返回列表
                </button>
              </div>
            )}
            {hiddenTotal > 0 && (
              <button
                type="button"
                className={`hidden-toggle ${showHidden ? "on" : ""}`}
                aria-pressed={showHidden}
                onClick={() => setShowHidden((value) => !value)}
              >
                <Icon name={showHidden ? "close" : "tray"} size={13} />
                {showHidden
                  ? "收起已隐藏案件"
                  : `展开已隐藏案件（${hiddenStats.claimed} 件已认领 · ${hiddenStats.stale} 件超期）`}
              </button>
            )}
            <div className="case-list" aria-busy={loading}>
              {listError ? (
                <EmptyState
                  icon="alert"
                  title="无法加载案件"
                  text={listError}
                  action={
                    <button className="btn btn-secondary" onClick={refresh}>
                      重试
                    </button>
                  }
                />
              ) : !data ? (
                <ListSkeleton />
              ) : (
                <>
                  {showHidden && hiddenTotal > 0 && (
                    <div className="hidden-group leading">
                      <p className="hidden-group-title">
                        已隐藏 {hiddenTotal} 件（
                        {[
                          hiddenStats.claimed
                            ? `${hiddenStats.claimed} 件已认领`
                            : "",
                          hiddenStats.stale
                            ? `${hiddenStats.stale} 件超过一周`
                            : "",
                        ]
                          .filter(Boolean)
                          .join("、")}
                        ）
                      </p>
                      {hiddenCases.length === 0 ? (
                        <p className="hidden-empty">本页没有隐藏案件。</p>
                      ) : (
                        hiddenCases.map((item) => (
                          <CaseRow
                            key={item.id}
                            item={item}
                            active={item.id === selectedId}
                            onSelect={() => setSelectedId(item.id)}
                          />
                        ))
                      )}
                      {data?.search !== true && (
                        <div className="hidden-footer">
                          <span>
                            第 {hiddenPage} / {hiddenTotalPages} 页
                          </span>
                          <div className="pager">
                            <button
                              className="icon-button"
                              aria-label="隐藏案件上一页"
                              disabled={hiddenPage <= 1 || loading}
                              onClick={() =>
                                setHiddenPage((value) => value - 1)
                              }
                            >
                              <Icon name="prev" size={16} />
                            </button>
                            <button
                              className="icon-button"
                              aria-label="隐藏案件下一页"
                              disabled={
                                hiddenPage >= hiddenTotalPages || loading
                              }
                              onClick={() =>
                                setHiddenPage((value) => value + 1)
                              }
                            >
                              <Icon name="next" size={16} />
                            </button>
                          </div>
                        </div>
                      )}
                    </div>
                  )}

                  {filtered.length === 0 ? (
                    <EmptyState
                      icon={
                        searchId !== null
                          ? "search"
                          : hiddenTotal > 0 && !query
                            ? "alert"
                            : "check"
                      }
                      title={
                        searchId !== null
                          ? hiddenTotal > 0
                            ? "该案件已隐藏"
                            : "没有找到这个案件"
                          : query
                            ? "没有匹配的案件"
                            : hiddenTotal > 0
                              ? "本页没有可审理的案件"
                              : "暂无待审核案件"
                      }
                      text={
                        searchId !== null
                          ? hiddenTotal > 0
                            ? `案件 #${searchId} 已被认领或超过一周，在上方隐藏组查看。`
                            : `上游未返回案件 #${searchId}，可能已处理或编号有误。`
                          : query
                            ? "关键词搜索仅覆盖当前页；输入纯数字案件号可全局搜索。"
                            : hiddenTotal > 0
                              ? `本页 ${[
                                  hiddenStats.claimed
                                    ? `${hiddenStats.claimed} 件已认领`
                                    : "",
                                  hiddenStats.stale
                                    ? `${hiddenStats.stale} 件超过一周`
                                    : "",
                                ]
                                  .filter(Boolean)
                                  .join("、")}，已自动隐藏。`
                              : "所有举报都已处理完毕。"
                      }
                    />
                  ) : (
                    filtered.map((item) => (
                      <CaseRow
                        key={item.id}
                        item={item}
                        active={item.id === selectedId}
                        onSelect={() => setSelectedId(item.id)}
                      />
                    ))
                  )}
                </>
              )}
            </div>
            <footer className="list-footer">
              <select
                aria-label="每页条数"
                value={pageSize}
                onChange={(event) => {
                  setPageSize(Number(event.target.value))
                  setPage(1)
                  setHiddenPage(1)
                }}
              >
                {[10, 20, 50].map((size) => (
                  <option key={size} value={size}>
                    {size} 条/页
                  </option>
                ))}
              </select>
              <div className="pager">
                <button
                  className="icon-button"
                  aria-label="上一页"
                  disabled={
                    page <=
                      1 ||
                    loading
                  }
                  onClick={() =>
                    setPage(
                      (value) =>
                        value -
                        1,
                    )
                  }
                >
                  <Icon name="prev" size={16} />
                </button>
                <span>
                  {page} / {totalPages}
                </span>
                <button
                  className="icon-button"
                  aria-label="下一页"
                  disabled={
                    page >=
                      totalPages ||
                    loading
                  }
                  onClick={() =>
                    setPage(
                      (value) =>
                        value +
                        1,
                    )
                  }
                >
                  <Icon name="next" size={16} />
                </button>
              </div>
            </footer>
          </section>

          <section className="detail-pane" aria-label="案件详情">
            {selectedId ===
            null ? (
              <EmptyState
                icon="tray"
                title="选择一个案件"
                text="从列表中选择案件，查看评论语境与举报记录。"
              />
            ) : (
              <ReviewPane
                key={selectedId}
                api={api}
                caseId={selectedId}
                mode={mode}
                onModeChange={setMode}
                aiEnabled={canUseAi}
                demo={demo}
                claimed={selectedClaimed}
                cachedSuggestion={suggestionCache.current.get(selectedId)}
                onSuggestion={(data) => suggestionCache.current.set(selectedId, data)}
                onBack={() => setSelectedId(null)}
                onSubmitted={handleSubmitted}
                onSubmitFailed={handleSubmitFailed}
                onError={fail}
                onStale={markStale}
              />
            )}
          </section>
        </>
      )}

      {settingsOpen && (
        <SettingsDialog
          settings={auto}
          api={api}
          loginToken={
            session.community?.hasToken ? session.community.masked : ""
          }
          aiEnabled={canUseAi}
          demo={demo}
          aiStatus={aiStatus}
          autoStatus={autoStatus}
          reviewerId={reviewerId}
          onReviewerIdChange={onReviewerIdChange}
          notify={notify}
          fail={fail}
          onChange={saveAuto}
          onClose={() => setSettingsOpen(false)}
        />
      )}

      {historyRecord && historyDetailRecord && (
        <HistoryDetailDialog
          api={api}
          record={historyDetailRecord}
          fetching={satisfactionFetching.has(historyDetailRecord.caseId)}
          onFetchSatisfaction={() => void fetchSatisfaction(historyDetailRecord)}
          onClose={() => setHistoryRecord(null)}
          onDelete={() => void removeRecord(historyDetailRecord)}
        />
      )}

      {toast && (
        <div className={`toast toast-${toast.tone}`} role="status">
          <Icon
            name={
              toast.tone ===
              "success"
                ? "check"
                : "alert"
            }
            size={16}
          />
          {toast.text}
        </div>
      )}
    </div>
  )
}

function SettingsDialog({
  settings,
  api,
  loginToken,
  aiEnabled,
  demo,
  aiStatus,
  autoStatus,
  reviewerId,
  onReviewerIdChange,
  notify,
  fail,
  onChange,
  onClose,
}: {
  settings: AutoSettings
  api: ReviewApi
  /** 当前社区凭证的掩码（空 = 尚未粘贴）。 */
  loginToken: string
  /** 后端已配置 AI 服务。 */
  aiEnabled: boolean
  demo: boolean
  aiStatus: AiStatus | null
  autoStatus: AutoStatus
  reviewerId: number
  onReviewerIdChange: (id: number) => void
  notify: (text: string, tone?: Tone) => void
  fail: (error: unknown) => void
  onChange: (next: AutoSettings) => void
  onClose: () => void
}) {
  const [reviewerText, setReviewerText] = useState(
    reviewerId > 0 ? String(reviewerId) : "",
  )
  const patch = (next: Partial<AutoSettings>) =>
    onChange({ ...settings, ...next })
  // 输入过程中不立刻收敛，失焦或回车时才写入，避免边打字边被夹到边界值。
  const [intervalText, setIntervalText] = useState(String(settings.interval))
  useEffect(() => {
    setIntervalText(String(settings.interval))
  }, [settings.interval])

  // ---- 社区 access-token：开源版唯一的凭证，直接粘贴 ----
  const [tokenInput, setTokenInput] = useState("")
  const [tokenBusy, setTokenBusy] = useState(false)

  async function saveToken() {
    const value = tokenInput.trim()
    if (!value) return notify("请粘贴 access-token", "error")
    setTokenBusy(true)
    localStorage.setItem(TOKEN_KEY, value)
    notify("凭证已保存，正在刷新…")
    await new Promise((resolve) => setTimeout(resolve, 900))
    window.location.reload()
  }

  // ---- 版本信息（设置最下方展示） ----
  const [about, setAbout] = useState<{ version: string; build: string } | null>(null)
  useEffect(() => {
    let cancelled = false
    api
      .version()
      .then((value) => !cancelled && setAbout(value))
      .catch(() => {})
    return () => {
      cancelled = true
    }
  }, [api])

  // ---- 服务器端 24 小时自动审核（仅服务器部署形态显示） ----
  const [serverAuto, setServerAuto] = useState<ServerAutoStatus | null>(null)
  const [autoToken, setAutoToken] = useState("")
  useEffect(() => {
    if (demo) return
    let cancelled = false
    api
      .serverAuto()
      .then((status) => !cancelled && setServerAuto(status))
      .catch((error) => !cancelled && console.warn("[server-auto]", messageOf(error)))
    return () => {
      cancelled = true
    }
  }, [api, demo])

  async function patchServerAuto(patch: ServerAutoPatch) {
    try {
      const next = await api.setServerAuto(patch)
      setServerAuto(next)
      return next
    } catch (error) {
      fail(error)
      return serverAuto
    }
  }

  async function commitAutoToken() {
    const value = autoToken.trim()
    if (!value) return
    setAutoToken("")
    await patchServerAuto({ token: value })
    notify("服务器自动审核 token 已保存")
  }

  function commitServerInterval() {
    const value = clampInterval(Number(intervalText))
    setIntervalText(String(value))
    if (serverAuto && value !== serverAuto.interval) void patchServerAuto({ interval: value })
  }

  function commitServerThreshold(value: number) {
    if (serverAuto && value !== serverAuto.threshold) void patchServerAuto({ threshold: value })
  }

  function commitServerPreferDelete(preferDelete: boolean) {
    if (serverAuto && preferDelete !== serverAuto.preferDelete)
      void patchServerAuto({ preferDelete })
  }

  async function toggleServerAuto(enabled: boolean) {
    if (enabled && !serverAuto?.configured && !autoToken.trim() && !loginToken) {
      notify(
        "还没有可用的审核凭证：在下方粘贴社区 access-token，或到设置里保存",
        "error",
      )
      return
    }
    const next = await patchServerAuto({ enabled })
    notify(
      next?.enabled
        ? "服务器 24 小时自动审核已开启，关闭浏览器也会继续运行"
        : "服务器 24 小时自动审核已关闭",
    )
  }

  function commitInterval() {
    const value = clampInterval(Number(intervalText))
    setIntervalText(String(value))
    if (value !== settings.interval) patch({ interval: value })
  }

  function commitReviewerId() {
    const value = Number(reviewerText.trim())
    const next = Number.isFinite(value) && value > 0 ? Math.floor(value) : 0
    setReviewerText(next > 0 ? String(next) : "")
    if (next === reviewerId) return
    onReviewerIdChange(next)
    notify(
      next > 0 ? `审核员 ID 已保存：${next}` : "审核员 ID 已清空",
    )
  }
  return (
    <div className="modal-backdrop" role="presentation" onClick={onClose}>
      <div
        className="modal"
        role="dialog"
        aria-modal="true"
        aria-label="审核设置"
        onClick={(event) => event.stopPropagation()}
      >
        <header className="modal-head">
          <h2>审核设置</h2>
          <button className="icon-button" aria-label="关闭" onClick={onClose}>
            <Icon name="close" size={16} />
          </button>
        </header>
        <div className="modal-body">
          {!demo && (
            <div className="setting-block">
              <h3>社区凭证</h3>
              <p className="setting-line">
                <Icon name="key" size={13} />
                <span>
                  {loginToken ? `已保存社区凭证（${loginToken}）` : "尚未保存社区凭证"}
                </span>
                <em>审核记录与偏好按 access-token 归属，只存在服务端 data 目录</em>
              </p>
              <label className="setting-row">
                <span>
                  <strong>社区 access-token</strong>
                  <em>
                    {loginToken
                      ? "已保存，重新粘贴可覆盖"
                      : "从社区站点复制 access-token 粘贴到这里"}
                  </em>
                </span>
                <input
                  className="setting-input"
                  type="password"
                  autoComplete="off"
                  spellCheck={false}
                  placeholder={loginToken ? "已保存" : "粘贴 access-token"}
                  value={tokenInput}
                  onChange={(event) => setTokenInput(event.target.value)}
                />
              </label>
              <label className="setting-row">
                <span>
                  <strong>审核员 ID</strong>
                  <em>
                    社区 checkerId；已审核历史按它同步，评论和作品审核共用
                  </em>
                </span>
                <input
                  className="setting-input"
                  inputMode="numeric"
                  placeholder="例如 3369552"
                  value={reviewerText}
                  onChange={(event) => setReviewerText(event.target.value)}
                  onBlur={commitReviewerId}
                  onKeyDown={(event) => {
                    if (event.key === "Enter") {
                      event.preventDefault()
                      event.currentTarget.blur()
                    }
                  }}
                />
              </label>
              <div className="setting-actions">
                <button
                  type="button"
                  className="btn btn-sm"
                  disabled={tokenBusy}
                  onClick={() => void saveToken()}
                >
                  {tokenBusy ? "保存中…" : "保存社区凭证"}
                </button>
              </div>
            </div>
          )}
          <SettingSwitch
            label="启用自动审核"
            hint={
              MOBILE_SHELL
                ? "手机客户端无法自动审核：系统会回收后台进程，请到电脑或服务器上开启"
                : aiEnabled
                  ? "定时拉取尚未归属审核员的案件，由 AI 判定后按下列规则处理"
                  : "后端未配置 AI 服务，无法启用"
            }
            checked={settings.enabled}
            disabled={!aiEnabled || MOBILE_SHELL}
            onChange={(enabled) => patch({ enabled })}
          />
          <SettingSwitch
            label="自动提交高置信结果"
            hint={
              MOBILE_SHELL
                ? "手机客户端无法后台自动提交：进程随时会被系统回收"
                : "关闭后 AI 只生成建议，所有案件都会进入待确认队列（下次启动会重新开启）"
            }
            checked={settings.autoSubmit}
            disabled={!settings.enabled || MOBILE_SHELL}
            onChange={(autoSubmit) => patch({ autoSubmit })}
          />
          <SettingSwitch
            label="优先删除而非替换"
            hint="AI 判定为「替换」时按「删除」提交，并改用删除版站内信"
            checked={settings.preferDelete}
            disabled={!settings.enabled}
            onChange={(preferDelete) => patch({ preferDelete })}
          />
          <label className="setting-row">
            <span>
              <strong>置信度阈值</strong>
              <em>低于该值的判定一律转人工确认</em>
            </span>
            <select
              value={settings.threshold}
              disabled={!settings.enabled}
              onChange={(event) =>
                patch({ threshold: Number(event.target.value) })
              }
            >
              {AUTO_THRESHOLDS.map((value) => (
                <option key={value} value={value}>
                  {Math.round(
                    value *
                      100,
                  )}
                  %
                </option>
              ))}
            </select>
          </label>
          <label className="setting-row">
            <span>
              <strong>拉取间隔</strong>
              <em>多久检查一次新案件，可填 {AUTO_INTERVAL_MIN}–{AUTO_INTERVAL_MAX} 秒</em>
            </span>
            <span className="number-field">
              <input
                type="number"
                inputMode="numeric"
                min={AUTO_INTERVAL_MIN}
                max={AUTO_INTERVAL_MAX}
                step={5}
                value={intervalText}
                disabled={!settings.enabled}
                onChange={(event) => setIntervalText(event.target.value)}
                onBlur={commitInterval}
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    event.preventDefault()
                    commitInterval()
                  }
                }}
              />
              <em>秒</em>
            </span>
          </label>
          <div className="setting-block">
            <h3>运行状态</h3>
            <p className="setting-line">
              <Icon
                name={
                  autoStatus.state ===
                  "error"
                    ? "alert"
                    : "bolt"
                }
                size={13}
              />
              <span>
                {autoStatus.state ===
                "running"
                  ? "正在检查新案件…"
                  : autoStatus.message}
              </span>
              <em>
                上次检查 {clockOf(autoStatus.lastRunAt)} · 已处理{" "}
                {autoStatus.submitted} 件
                {autoStatus.candidates > 0
                  ? ` · 待处理 ${autoStatus.candidates} 件`
                  : ""}
                {autoStatus.stale > 0
                  ? ` · ${autoStatus.stale} 件超过一周`
                  : ""}
                {autoStatus.cooling >
                0
                  ? ` · ${autoStatus.cooling} 件退避中`
                  : ""}
              </em>
            </p>
          </div>
          <div className="setting-block">
            <h3>AI 密钥</h3>
            {!aiStatus ||
            !aiStatus.enabled ? (
              <p className="setting-line">
                <Icon name="key" size={13} />
                <span>后端未配置 AI 服务</span>
              </p>
            ) : (
              <>
                {aiStatus.keys.map((key) => (
                  <p className="setting-line" key={key.index}>
                    <Icon name="key" size={13} />
                    <span className="key-mask">
                      #{key.index} {key.masked}
                    </span>
                    <em>
                      {key.state === "healthy"
                        ? `可用 ${key.remaining}/${key.rateLimit}`
                        : key.state === "throttled"
                          ? "本分钟已用满"
                          : `冷却 ${Math.max(
                              1,
                              Math.ceil(
                                key.cooldownSecs /
                                  60,
                              ),
                            )} 分`}
                      {key.failures >
                      0
                        ? ` · 失败 ${key.failures} 次`
                        : ""}
                    </em>
                  </p>
                ))}
                <p className="setting-note">
                  共 {aiStatus.keys.length} 个密钥，失败后自动切换下一个
                  {aiStatus.keySource ===
                  "builtin"
                    ? " · 已打包进程序"
                    : ""}
                  {aiStatus.model ? ` · 模型 ${aiStatus.model}` : ""}
                </p>
              </>
            )}
          </div>
          {!demo && (
            <div className="setting-block">
              <h3>服务器 24 小时自动审核</h3>
              {MOBILE_SHELL ? (
                <SettingSwitch
                  label="开启 24 小时无人值守"
                  hint="手机客户端无法运行值守：系统会回收后台进程，请到电脑或服务器上开启"
                  checked={false}
                  disabled
                  onChange={() => {}}
                />
              ) : !serverAuto ? (
                <p className="setting-line">
                  <span>正在读取服务器配置…</span>
                </p>
              ) : (
                <>
                  <SettingSwitch
                    label="开启 24 小时无人值守"
                    hint="由服务器定时拉取案件并自动提交，关掉浏览器也继续运行；低置信度案件仍会记录等你人工处理"
                    checked={serverAuto.enabled}
                    onChange={(enabled) => void toggleServerAuto(enabled)}
                  />
                  <div className="setting-row">
                    <span>
                      <strong>审核 token</strong>
                      <em>
                        {serverAuto.configured
                          ? `已配置（${serverAuto.tokenMasked}），可填写新值覆盖`
                          : "提交案件使用的审核员身份，留空沿用已保存的 token"}
                      </em>
                    </span>
                    <span className="number-field">
                      <input
                        type="password"
                        autoComplete="off"
                        spellCheck={false}
                        placeholder="access-token"
                        style={{ width: 150, textAlign: "left" }}
                        value={autoToken}
                        onChange={(event) => setAutoToken(event.target.value)}
                        onBlur={commitAutoToken}
                        onKeyDown={(event) => {
                          if (event.key === "Enter") {
                            event.preventDefault()
                            void commitAutoToken()
                          }
                        }}
                      />
                    </span>
                  </div>
                  <label className="setting-row">
                    <span>
                      <strong>拉取间隔</strong>
                      <em>服务器多久检查一轮新案件</em>
                    </span>
                    <span className="number-field">
                      <input
                        type="number"
                        inputMode="numeric"
                        min={AUTO_INTERVAL_MIN}
                        max={AUTO_INTERVAL_MAX}
                        step={5}
                        value={intervalText}
                        onChange={(event) => setIntervalText(event.target.value)}
                        onBlur={commitServerInterval}
                        onKeyDown={(event) => {
                          if (event.key === "Enter") {
                            event.preventDefault()
                            commitServerInterval()
                          }
                        }}
                      />
                      <em>秒</em>
                    </span>
                  </label>
                  <label className="setting-row">
                    <span>
                      <strong>置信度阈值</strong>
                      <em>低于该值的判定不提交，记录后等人工确认</em>
                    </span>
                    <select
                      value={serverAuto.threshold}
                      onChange={(event) =>
                        commitServerThreshold(Number(event.target.value))
                      }
                    >
                      {AUTO_THRESHOLDS.map((value) => (
                        <option key={value} value={value}>
                          {Math.round(value * 100)}%
                        </option>
                      ))}
                    </select>
                  </label>
                  <SettingSwitch
                    label="优先删除而非替换"
                    hint="AI 判定为「替换」时按「删除」提交"
                    checked={serverAuto.preferDelete}
                    onChange={commitServerPreferDelete}
                  />
                  <p className="setting-line">
                    <Icon name={serverAuto.enabled ? "bolt" : "clock"} size={13} />
                    <span>
                      {serverAuto.enabled
                        ? serverAuto.lastMessage || "等待下一轮检查"
                        : "已暂停"}
                    </span>
                    <em>
                      {serverAuto.lastRunAt
                        ? `上次检查 ${clockOf(serverAuto.lastRunAt)}`
                        : "尚未运行"}
                      {` · 今日自动 ${serverAuto.today.submitted} 件 · 转人工 ${serverAuto.today.waiting} 件 · 失败 ${serverAuto.today.failed} 件`}
                    </em>
                  </p>
                  <p className="setting-note">
                    配置保存在服务器 auto.json，重启后自动恢复；所有动作记录在
                    qingfeng.log 与容器日志中。
                  </p>
                </>
              )}
            </div>
          )}
        </div>
        <footer className="modal-foot">
          <p>
            <Icon name="alert" size={14} />
            {demo
              ? "示例模式只在本地演示，不会提交到社区"
              : "自动提交会直接受理真实举报，请确认规则后再启用"}
          </p>
          <div className="modal-foot-right">
            <span className="app-foot">
              版本 v{about?.version ?? "—"}
              <a
                href="https://beian.miit.gov.cn/"
                target="_blank"
                rel="noreferrer"
              >
                皖ICP备2026007802号-1
              </a>
            </span>
            <button className="btn btn-primary" onClick={onClose}>
              完成
            </button>
          </div>
        </footer>
      </div>
    </div>
  )
}

function SettingSwitch({
  label,
  hint,
  checked,
  disabled,
  onChange,
}: {
  label: string
  hint: string
  checked: boolean
  disabled?: boolean
  onChange: (value: boolean) => void
}) {
  return (
    <label className={`setting-row ${disabled ? "disabled" : ""}`}>
      <span>
        <strong>{label}</strong>
        <em>{hint}</em>
      </span>
      <input
        type="checkbox"
        checked={checked}
        disabled={disabled}
        onChange={(event) => onChange(event.target.checked)}
      />
      <span className="switch" aria-hidden="true" />
    </label>
  )
}

function CaseRow({
  item,
  active,
  onSelect,
}: {
  item: CaseSummary
  active: boolean
  onSelect: () => void
}) {
  const contentRef = useRef<HTMLSpanElement>(null)
  const [expanded, setExpanded] = useState(false)
  const [foldable, setFoldable] = useState(false)
  useEffect(() => {
    const element = contentRef.current
    if (!element || expanded) return
    setFoldable(element.scrollHeight > element.clientHeight + 1)
  }, [item.content, expanded])

  return (
    <div
      className={`case-row ${active ? "active" : ""}`}
      role="button"
      tabIndex={0}
      aria-current={active || undefined}
      onClick={onSelect}
      onKeyDown={(event) => {
        if (event.key !== "Enter" && event.key !== " ") return
        event.preventDefault()
        onSelect()
      }}
    >
      <Avatar name={item.nickname || "用"} />
      <div className="case-row-body">
        <div className="case-row-top">
          {item.userId ? (
            <a
              className="case-row-name"
              href={profileUrl(item.userId)}
              target="_blank"
              rel="noreferrer"
              onClick={(event) => event.stopPropagation()}
            >
              {item.nickname || "未知用户"}
            </a>
          ) : (
            <strong>{item.nickname || "未知用户"}</strong>
          )}
          <span>#{item.id}</span>
        </div>
        <span
          ref={contentRef}
          className={
            expanded ? "case-row-content expanded" : "case-row-content"
          }
        >
          {item.content || "（空评论）"}
        </span>
        {foldable && (
          <button
            type="button"
            className="case-row-expand"
            onClick={(event) => {
              event.stopPropagation()
              setExpanded((value) => !value)
            }}
          >
            {expanded ? "收起" : "展开全文"}
          </button>
        )}
        <span className="case-row-meta">
          {unclaimed(item.checkerName) ? (
            <>
              <span className="case-badge case-badge-open">
                <Icon name="tray" size={11} />
                尚未认领
              </span>
              {isStale(parseWhen(item.createdAt) ?? null) && (
                <span className="case-badge case-badge-stale">
                  <Icon name="alert" size={11} />
                  超过一周
                </span>
              )}
            </>
          ) : (
            <span className="case-badge case-badge-claimed">
              <Icon name="check" size={11} />
              审核员 {item.checkerName}
            </span>
          )}
        </span>
        <div className="case-row-actions">
          {item.compositionId ? (
            <a
              className="link-pill"
              href={sceneUrl(item.compositionId)}
              target="_blank"
              rel="noreferrer"
              onClick={(event) => event.stopPropagation()}
            >
              案发现场
              <Icon name="external" size={12} />
            </a>
          ) : (
            <span />
          )}
          <button
            type="button"
            className="btn btn-secondary btn-sm"
            onClick={(event) => {
              event.stopPropagation()
              onSelect()
            }}
          >
            开始审核
          </button>
        </div>
      </div>
    </div>
  )
}

function HistoryPane({
  records,
  stats,
  fetching,
  syncing,
  syncedAt,
  onRefresh,
  onSyncHistory,
  onFetchSatisfaction,
  onClear,
  onOpen,
  onDelete,
}: {
  records: RecordEntry[]
  stats: DayStats
  /** 正在手动获取满意度的案件号。 */
  fetching: Set<number>
  /** 正在从已审核案件列表同步历史。 */
  syncing: boolean
  /** 上次同步完成时间（epoch 毫秒）。 */
  syncedAt: number
  onRefresh: () => void
  onSyncHistory: () => void
  onFetchSatisfaction: (record: RecordEntry) => void
  onClear: () => void
  onOpen: (record: RecordEntry) => void
  onDelete: (record: RecordEntry) => void
}) {
  const syncedLabel = syncedAt
    ? new Date(syncedAt).toLocaleTimeString("zh-CN", {
        hour: "2-digit",
        minute: "2-digit",
      })
    : ""
  return (
    <section className="history-pane" aria-label="历史审核">
      <header className="list-header">
        <div className="list-title">
          <h1>历史审核</h1>
          <span>
            {"服务端本机审核记录（只对本 token 可见） · 满意度满 24 小时后可手动获取，也会每天自动刷新"}
            {syncing
              ? " · 正在从案件列表同步…"
              : syncedLabel
                ? ` · 上次同步 ${syncedLabel}`
                : ""}
          </span>
        </div>
        <div className="list-actions">
          <button
            className="icon-button"
            aria-label="从案件列表同步历史"
            title={
              syncing
                ? "正在从案件列表同步…"
                : syncedLabel
                  ? `从案件列表同步历史（上次 ${syncedLabel}）`
                  : "从案件列表同步历史"
            }
            onClick={onSyncHistory}
            disabled={syncing}
          >
            <Icon name="tray" size={17} />
          </button>
          <button
            className="icon-button"
            aria-label="刷新全部满意度"
            title="刷新全部满意度（只刷新已满 24 小时的记录）"
            onClick={onRefresh}
          >
            <Icon name="refresh" size={17} />
          </button>
          <button
            className="icon-button"
            aria-label="清空本地历史"
            title="清空本机缓存（服务端记录不受影响）"
            onClick={onClear}
            disabled={records.length === 0}
          >
            <Icon name="close" size={17} />
          </button>
        </div>
      </header>
      <div className="stats-bar" role="group" aria-label="今日统计">
        <div className="stat-cell stat-blue">
          <Icon name="bolt" size={14} />
          <strong>{stats.submitted}</strong>
          <span>自动处理</span>
        </div>
        <div className="stat-cell stat-orange">
          <Icon name="alert" size={14} />
          <strong>{stats.waiting}</strong>
          <span>转人工</span>
        </div>
        <div className="stat-cell stat-green">
          <Icon name="check" size={14} />
          <strong>{stats.manual}</strong>
          <span>手动提交</span>
        </div>
        <div className="stat-cell">
          <Icon name="clock" size={14} />
          <strong>{records.length}</strong>
          <span>历史记录</span>
        </div>
      </div>
      <div className="history-list">
        {records.length === 0 ? (
          <EmptyState
            icon="clock"
            title="还没有审核记录"
            text="提交案件后会记在本机，满 24 小时后可以手动获取满意度。"
          />
        ) : (
          records.map((record) => (
            <HistoryRow
              key={`${record.caseId}-${record.createdAt}`}
              record={record}
              fetching={fetching.has(record.caseId)}
              onOpen={() => onOpen(record)}
              onFetchSatisfaction={() => onFetchSatisfaction(record)}
              onDelete={() => onDelete(record)}
            />
          ))
        )}
      </div>
    </section>
  )
}

function HistoryRow({
  record,
  fetching,
  onOpen,
  onFetchSatisfaction,
  onDelete,
}: {
  record: RecordEntry
  fetching: boolean
  onOpen: () => void
  onFetchSatisfaction: () => void
  onDelete: () => void
}) {
  const normalDisplay = Number(record.action) === 1
  const ready = Date.now() - record.createdAt >= SATISFACTION_DELAY_MS
  const canFetch = !normalDisplay && ready
  const satisfaction = normalDisplay
    ? "正常显示不统计满意度"
    : record.satisfactionFetched
      ? record.satisfaction === null
        ? "满意度未知"
        : `满意度 ${Math.round(record.satisfaction * 100)}%`
      : !ready
        ? "明天可看满意度"
        : record.judgeRecordId > 0
          ? "满意度待获取"
          : "等待判决记录同步"
  const when = new Date(record.createdAt).toLocaleString("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  })
  return (
    <div
      className="history-row"
      role="button"
      tabIndex={0}
      title="查看案件详情"
      onClick={onOpen}
      onKeyDown={(event) => {
        if (event.key !== "Enter" && event.key !== " ") return
        event.preventDefault()
        onOpen()
      }}
    >
      <div className="history-top">
        <strong>{record.nickname || "未知用户"}</strong>
        <span className="history-id">#{record.caseId}</span>
        {record.action && (
          <span className={`case-badge ${ACTION_TONE[record.action]}`}>
            {actionLabel(record.action)}
          </span>
        )}
        {record.source === "auto" && (
          <span className="case-badge case-badge-open">自动</span>
        )}
        {record.source === "synced" && (
          <span className="case-badge case-badge-open">同步</span>
        )}
        <span className="history-time">{when}</span>
        {record.source !== "synced" && (
          <button
            type="button"
            className="icon-button history-delete"
            aria-label={`删除案件 #${record.caseId} 的记录`}
            title="删除这条历史记录"
            onClick={(event) => {
              event.stopPropagation()
              onDelete()
            }}
          >
            <Icon name="trash" size={15} />
          </button>
        )}
      </div>
      <p className="history-content">{record.content || "（空评论）"}</p>
      <div className="history-meta">
        {record.tokenMasked && (
          <span className="history-token" title="操作 token">
            token {record.tokenMasked}
          </span>
        )}
        {record.caseReportTime && (
          <span className="history-report-time">
            举报于 {record.caseReportTime}
          </span>
        )}
        {record.pending && <span className="history-pending">仅本机</span>}
        <span className="history-hint">查看详情</span>
      </div>
      <div className="history-satisfaction-line">
        <span
          className={`history-satisfaction ${
            record.satisfactionFetched && record.satisfaction !== null
              ? "has-value"
              : ""
          }`}
        >
          {satisfaction}
        </span>
        {canFetch && (
          <button
            type="button"
            className="link-pill history-satisfaction-fetch"
            disabled={fetching}
            title={
              record.satisfactionFetched
                ? "重新获取满意度"
                : "手动获取满意度"
            }
            onClick={(event) => {
              event.stopPropagation()
              onFetchSatisfaction()
            }}
          >
            {fetching
              ? "获取中…"
              : record.satisfactionFetched
                ? "刷新"
                : "获取"}
          </button>
        )}
      </div>
    </div>
  )
}

/** 历史案件详情：优先拉上游最新详情，案件已消失时回退到本地记录。 */
function HistoryDetailDialog({
  api,
  record,
  fetching,
  onFetchSatisfaction,
  onClose,
  onDelete,
}: {
  api: ReviewApi
  record: RecordEntry
  fetching: boolean
  onFetchSatisfaction: () => void
  onClose: () => void
  onDelete: () => void
}) {
  const [detail, setDetail] = useState<CaseDetail | null>(null)
  const [error, setError] = useState("")

  useEffect(() => {
    let alive = true
    setDetail(null)
    setError("")
    api
      .detail(record.caseId)
      .then((result) => {
        if (alive) setDetail(result)
      })
      .catch((err) => {
        if (alive) setError(messageOf(err))
      })
    return () => {
      alive = false
    }
  }, [api, record.caseId])

  const normalDisplay = Number(record.action) === 1
  const ready = Date.now() - record.createdAt >= SATISFACTION_DELAY_MS
  const satisfaction = normalDisplay
    ? "正常显示不统计满意度"
    : record.satisfactionFetched
      ? record.satisfaction === null
        ? "满意度未知"
        : `满意度 ${Math.round(record.satisfaction * 100)}%`
      : ready
        ? "满意度尚未获取"
        : "提交满 24 小时后可获取"

  return (
    <div className="modal-backdrop" role="presentation" onClick={onClose}>
      <div
        className="modal history-detail"
        role="dialog"
        aria-modal="true"
        aria-label={`案件 #${record.caseId} 历史详情`}
        onClick={(event) => event.stopPropagation()}
      >
        <header className="modal-head">
          <h2>案件 #{record.caseId}</h2>
          <button className="icon-button" aria-label="关闭" onClick={onClose}>
            <Icon name="close" size={16} />
          </button>
        </header>
        <div className="modal-body">
          <div className="history-detail-meta">
            {record.action && (
              <span className={`case-badge ${ACTION_TONE[record.action]}`}>
                {actionLabel(record.action)}
              </span>
            )}
            {record.source === "auto" && (
              <span className="case-badge case-badge-open">自动</span>
            )}
            {record.source === "synced" && (
              <span className="case-badge case-badge-open">同步</span>
            )}
            <span>{record.nickname || "未知用户"}</span>
            {record.caseReportTime && (
              <span>举报于 {record.caseReportTime}</span>
            )}
            <span>
              审核于{" "}
              {new Date(record.createdAt).toLocaleString("zh-CN", {
                year: "numeric",
                month: "2-digit",
                day: "2-digit",
                hour: "2-digit",
                minute: "2-digit",
              })}
            </span>
            <span>{satisfaction}</span>
            {!normalDisplay && ready && (
              <button
                type="button"
                className="link-pill"
                disabled={fetching}
                onClick={onFetchSatisfaction}
              >
                {fetching
                  ? "获取中…"
                  : record.satisfactionFetched
                    ? "刷新满意度"
                    : "获取满意度"}
              </button>
            )}
          </div>

          {record.remark && (
            <div className="history-detail-block">
              <h3>受理备注</h3>
              <p className="muted-block">{record.remark}</p>
            </div>
          )}

          <div className="history-detail-block">
            <h3>被举报评论</h3>
            {error ? (
              <p className="muted-block">
                案件详情已不可用（{error}）。记录内容：
                {record.content || "（空评论）"}
              </p>
            ) : !detail ? (
              <p className="muted-block">正在加载案件详情…</p>
            ) : (
              <div className="comment">
                <div className="comment-author">
                  <Avatar
                    src={detail.author.avatar}
                    name={detail.author.nickname || "用"}
                  />
                  <div>
                    <strong>{detail.author.nickname || "未知用户"}</strong>
                    <span>
                      ID {detail.author.userId || "未知"}
                      {detail.commentCreatedAt &&
                        ` · ${detail.commentCreatedAt}`}
                    </span>
                  </div>
                </div>
                <p className="comment-text">{detail.content || "（空评论）"}</p>
              </div>
            )}
          </div>

          {detail && (
            <>
              <div className="history-detail-block">
                <h3>案发现场</h3>
                <div className="info-row">
                  <span>作品</span>
                  <strong>{detail.composition.title || "未命名作品"}</strong>
                </div>
              </div>
              <div className="history-detail-block">
                <h3>举报记录（{detail.reportNum} 次）</h3>
                {detail.reports.map((report) => (
                  <div className="report" key={report.id}>
                    <Avatar
                      src={report.reporter.avatar}
                      name={report.reporter.nickname || "用"}
                      size="sm"
                    />
                    <div className="report-body">
                      <div className="report-top">
                        <strong>
                          {report.reporter.nickname || "匿名用户"}
                        </strong>
                        <span>{report.createdAt}</span>
                      </div>
                      <span className="badge">
                        {report.reasonName || "未分类"}
                      </span>
                      <p className="report-text">
                        {report.content || "未填写举报描述"}
                      </p>
                      {report.images.length > 0 && (
                        <div className="report-images">
                          {report.images.map((src) => (
                            <a
                              key={src}
                              href={src}
                              target="_blank"
                              rel="noreferrer"
                            >
                              <img src={src} alt="举报凭证" loading="lazy" />
                            </a>
                          ))}
                        </div>
                      )}
                    </div>
                  </div>
                ))}
              </div>
            </>
          )}
        </div>
        <footer className="modal-foot">
          <p>
            <Icon name="alert" size={14} />
            {record.source === "synced"
              ? "该记录来自已审核案件列表，会随同步更新"
              : "删除后服务端审核记录也会一起移除，无法恢复"}
          </p>
          <div className="modal-foot-right">
            <button className="btn btn-secondary" onClick={onClose}>
              关闭
            </button>
            {record.source !== "synced" && (
              <button className="btn btn-danger" onClick={onDelete}>
                <Icon name="trash" size={14} />
                删除记录
              </button>
            )}
          </div>
        </footer>
      </div>
    </div>
  )
}

function ListSkeleton() {  return (
    <div aria-hidden="true">
      {Array.from({ length: 6 }, (_, index) => (
        <div className="skeleton-row" key={index}>
          <span className="sk sk-avatar" />
          <span className="sk-lines">
            <span className="sk sk-line short" />
            <span className="sk sk-line" />
          </span>
        </div>
      ))}
    </div>
  )
}

function ReviewPane({
  api,
  caseId,
  mode,
  onModeChange,
  aiEnabled,
  demo,
  claimed,
  cachedSuggestion,
  onSuggestion,
  onBack,
  onSubmitted,
  onSubmitFailed,
  onError,
  onStale,
}: {
  api: ReviewApi
  caseId: number
  mode: Mode
  onModeChange: (mode: Mode) => void
  aiEnabled: boolean
  demo: boolean
  /** 案件已被别的审核员认领：不再花配额分析。null = 列表里没有该案件，退回详情判断。 */
  claimed: boolean | null
  cachedSuggestion?: Suggestion
  onSuggestion: (data: Suggestion) => void
  onBack: () => void
  onSubmitted: (
    id: number,
    info: { action: Action; nickname: string; content: string },
  ) => void
  onSubmitFailed: (caseId: number, error: unknown) => void
  onError: (error: unknown) => void
  /** 详情确认举报时间超过一周：让列表把它移入「已隐藏」。 */
  onStale: (id: number) => void
}) {
  const [detail, setDetail] = useState<CaseDetail | null>(null)
  const [loadError, setLoadError] = useState("")
  const [suggestion, setSuggestion] = useState<SuggestionState>(
    cachedSuggestion
      ? { status: "ready", data: cachedSuggestion }
      : { status: "idle" },
  )
  const [form, setForm] = useState<FormState>(
    cachedSuggestion
      ? {
          action: cachedSuggestion.action,
          remark: cachedSuggestion.remark,
          userMessage: cachedSuggestion.userMessage,
          reporterMessage: cachedSuggestion.reporterMessage,
        }
      : EMPTY_FORM,
  )
  const [formError, setFormError] = useState("")
  const [submitting, setSubmitting] = useState(false)
  // 举报超过一周或时间无法确认：只展示详情，不再分析/提交。
  const [staleCase, setStaleCase] = useState(false)
  const edited = useRef(false)
  const alive = useRef(true)
  // 只读状态以列表为准：多举报案件里详情自带的 checkerId / isDeal 属于旧的那条举报，
  // 拿来判断整案会把还没人认领的案件锁成「只可查看」。列表里没有该案件时才退回详情字段。
  const claimedByDetail = Boolean(detail?.isDeal) || Boolean(detail?.checkerId)
  const claimedNow = claimed === null ? claimedByDetail : claimed
  // 已经被认领或已处理的案件只能查看，不能再改动或提交。
  const readOnly = claimedNow || staleCase

  useEffect(() => {
    alive.current = true
    return () => {
      alive.current = false
    }
  }, [])

  const loadDetail = useCallback(() => {
    setLoadError("")
    setDetail(null)
    setStaleCase(false)
    api
      .detail(caseId)
      .then((result) => {
        if (!alive.current) return
        setDetail(result)
        const age = caseAgeOf(result)
        const stale = age === null || isStale(age)
        setStaleCase(stale)
        if (stale) onStale(caseId)
      })
      .catch((error) => {
        if (!alive.current) return
        if (isExpired(error)) onError(error)
        else setLoadError(messageOf(error))
      })
  }, [api, caseId, onError, onStale])

  useEffect(loadDetail, [loadDetail])

  const applySuggestion = useCallback((data: Suggestion) => {
    edited.current = false
    setFormError("")
    setForm({
      action: data.action,
      remark: data.remark,
      userMessage: data.userMessage,
      reporterMessage: data.reporterMessage,
    })
  }, [])

  // 用 ref 持有回调，避免父组件重渲染导致 requestSuggestion 变化、effect 重跑。
  const suggestionSink = useRef(onSuggestion)
  suggestionSink.current = onSuggestion

  const requestSuggestion = useCallback(() => {
    setSuggestion({ status: "loading" })
    api
      .suggest(caseId)
      .then((data) => {
        if (!alive.current) return
        setSuggestion({ status: "ready", data })
        // 写回父组件的缓存，切走再切回来直接复用，不再消耗模型配额。
        suggestionSink.current(data)
        if (!edited.current) applySuggestion(data)
      })
      .catch((error) => {
        if (!alive.current) return
        if (isExpired(error)) onError(error)
        else setSuggestion({ status: "error", message: messageOf(error) })
      })
  }, [api, caseId, applySuggestion, onError])

  useEffect(() => {
    // 已认领或已处理的案件不再自动分析，省下模型配额。
    if (readOnly) return
    if (mode === "ai" && aiEnabled && detail && suggestion.status === "idle")
      requestSuggestion()
  }, [readOnly, mode, aiEnabled, detail, suggestion.status, requestSuggestion])

  function update(patch: Partial<FormState>) {
    if (readOnly) return
    edited.current = true
    setFormError("")
    setForm((current) => ({ ...current, ...patch }))
  }

  function chooseAction(action: Action) {
    if (readOnly) return
    edited.current = true
    setFormError("")
    setForm((current) => ({
      ...current,
      action,
      userMessage: isTemplate(current.userMessage, "user")
        ? TEMPLATES[action].user
        : current.userMessage,
      reporterMessage: isTemplate(current.reporterMessage, "reporter")
        ? TEMPLATES[action].reporter
        : current.reporterMessage,
    }))
  }

  async function submit(event: FormEvent) {
    event.preventDefault()
    if (submitting || !detail || readOnly) return
    if (!form.action) return setFormError("请选择受理结果")
    if (
      !form.remark.trim() ||
      !form.userMessage.trim() ||
      !form.reporterMessage.trim()
    ) {
      return setFormError("请填写受理备注和两条站内信通知")
    }
    setSubmitting(true)
    try {
      await api.verify(caseId, {
        action: form.action,
        remark: form.remark.trim(),
        userMessage: form.userMessage.trim(),
        reporterMessage: form.reporterMessage.trim(),
      })
      onSubmitted(caseId, {
        action: form.action,
        nickname: detail.author.nickname,
        content: detail.content,
      })
    } catch (error) {
      if (alive.current) setSubmitting(false)
      onSubmitFailed(caseId, error)
    }
  }

  return (
    <div className="review">
      <header className="detail-toolbar">
        <button
          className="icon-button mobile-only"
          aria-label="返回列表"
          onClick={onBack}
        >
          <Icon name="back" size={18} />
        </button>
        <div className="detail-title">
          <strong>案件 #{caseId}</strong>
          <span>
            {detail
              ? `${detail.reports.length} 条举报 · ${
                  detail.isDeal ? "你已处理过" : "待处理"
                }`
              : "正在加载"}
          </span>
        </div>
        <Segmented
          label="审核模式"
          value={mode}
          onChange={onModeChange}
          options={[
            { value: "ai", label: "AI 辅助" },
            { value: "manual", label: "人工审核" },
          ]}
        />
      </header>

      <div className="detail-scroll">
        {loadError ? (
          <EmptyState
            icon="alert"
            title="无法加载案件详情"
            text={loadError}
            action={
              <button className="btn btn-secondary" onClick={loadDetail}>
                重试
              </button>
            }
          />
        ) : !detail ? (
          <DetailSkeleton />
        ) : (
          <div className="detail-content">
            <Group
              title="被举报评论"
              aside={
                detail.author.userId && (
                  <ExternalLink href={profileUrl(detail.author.userId)}>
                    用户主页
                  </ExternalLink>
                )
              }
            >
              <div className="comment">
                <div className="comment-author">
                  <Avatar
                    src={detail.author.avatar}
                    name={detail.author.nickname || "用"}
                  />
                  <div>
                    <strong>{detail.author.nickname || "未知用户"}</strong>
                    <span>
                      ID {detail.author.userId || "未知"}
                      {detail.commentCreatedAt &&
                        ` · ${detail.commentCreatedAt}`}
                    </span>
                  </div>
                </div>
                <p className="comment-text">{detail.content || "（空评论）"}</p>
              </div>
            </Group>

            <Group
              title="案发现场"
              aside={
                detail.composition.compositionId && (
                  <ExternalLink
                    href={sceneUrl(detail.composition.compositionId)}
                  >
                    打开作品
                  </ExternalLink>
                )
              }
            >
              <div className="info-row">
                <span>作品</span>
                <strong>{detail.composition.title || "未命名作品"}</strong>
              </div>
              {detail.composition.description && (
                <p className="muted-block">{detail.composition.description}</p>
              )}
              {detail.composition.tags.length > 0 && (
                <div className="tags">
                  {detail.composition.tags.map((tag) => (
                    <span className="tag" key={tag}>
                      {tag}
                    </span>
                  ))}
                </div>
              )}
            </Group>

            <Group
              title="举报记录"
              aside={
                <span className="group-aside">
                  共 {detail.reportNum} 次 · 提交时全部受理
                </span>
              }
            >
              {detail.reports.map((report) => (
                <div className="report" key={report.id}>
                  <Avatar
                    src={report.reporter.avatar}
                    name={report.reporter.nickname || "用"}
                    size="sm"
                  />
                  <div className="report-body">
                    <div className="report-top">
                      <strong>{report.reporter.nickname || "匿名用户"}</strong>
                      <span>{report.createdAt}</span>
                    </div>
                    <span className="badge">
                      {report.reasonName || "未分类"}
                    </span>
                    <p className="report-text">
                      {report.content || "未填写举报描述"}
                    </p>
                    {report.images.length > 0 && (
                      <div className="report-images">
                        {report.images.map((src) => (
                          <a
                            key={src}
                            href={src}
                            target="_blank"
                            rel="noreferrer"
                          >
                            <img src={src} alt="举报凭证" loading="lazy" />
                          </a>
                        ))}
                      </div>
                    )}
                  </div>
                  <span className="status-pill">未处理</span>
                </div>
              ))}
            </Group>

            {detail.judgeRecords !== null &&
              detail.judgeRecords !== undefined && (
                <Group title="其他委员判案记录">
                  <pre className="judge-records">
                    {JSON.stringify(detail.judgeRecords, null, 2)}
                  </pre>
                </Group>
              )}

            {mode === "ai" && !staleCase && (
              <AiCard
                enabled={aiEnabled}
                claimed={claimedNow}
                state={suggestion}
                onRetry={requestSuggestion}
                onApply={applySuggestion}
              />
            )}

            <form className="review-form" onSubmit={submit}>
              <Group title="受理">
                {readOnly && (
                  <p className="readonly-note" role="status">
                    <Icon name={staleCase ? "clock" : "check"} size={15} />
                    {staleCase
                      ? "该案件举报已超过一周或举报时间无法确认，系统不会再分析或提交。"
                      : "该案件已被认领，仅可查看，不能再提交或修改。"}
                  </p>
                )}
                <div className="form-row">
                  <span className="form-label">受理结果</span>
                  <Segmented
                    label="受理结果"
                    value={form.action}
                    onChange={chooseAction}
                    options={ACTIONS}
                    disabled={readOnly}
                  />
                </div>
                <TextArea
                  label="受理备注"
                  hint="将公示于公告栏"
                  rows={2}
                  value={form.remark}
                  placeholder="例如：违规散播存档码"
                  disabled={readOnly}
                  onChange={(remark) => update({ remark })}
                />
                <TextArea
                  label="举报人站内信"
                  value={form.reporterMessage}
                  placeholder="选择受理结果后自动填入默认文案"
                  disabled={readOnly}
                  onChange={(reporterMessage) => update({ reporterMessage })}
                />
                <TextArea
                  label="被举报人站内信"
                  value={form.userMessage}
                  placeholder="选择受理结果后自动填入默认文案"
                  disabled={readOnly}
                  onChange={(userMessage) => update({ userMessage })}
                />
              </Group>
              {formError && (
                <p className="form-error" role="alert">
                  <Icon name="alert" size={15} />
                  {formError}
                </p>
              )}
              <div className="submit-bar">
                <span>
                  <Icon name="shield" size={15} />
                  {readOnly
                    ? staleCase
                      ? "超期案件只可查看"
                      : "已被认领，只可查看"
                    : demo
                      ? "示例模式不会提交到社区"
                      : "提交后案件将归属当前审核员"}
                </span>
                <button
                  className="btn btn-primary btn-large"
                  disabled={submitting || readOnly}
                >
                  {readOnly
                    ? staleCase
                      ? "超期案件"
                      : "案件已处理"
                    : submitting
                      ? "正在提交…"
                      : "提交审核结果"}
                </button>
              </div>
            </form>
          </div>
        )}
      </div>
    </div>
  )
}

function TextArea({
  label,
  hint,
  value,
  placeholder,
  rows = 3,
  disabled = false,
  onChange,
}: {
  label: string
  hint?: string
  value: string
  placeholder: string
  rows?: number
  disabled?: boolean
  onChange: (value: string) => void
}) {
  return (
    <label className="textarea-field">
      <span className="form-label">
        {label}
        {hint && <em>{hint}</em>}
      </span>
      <textarea
        rows={rows}
        value={value}
        placeholder={placeholder}
        disabled={disabled}
        onChange={(event) => onChange(event.target.value)}
      />
    </label>
  )
}

function AiCard({
  enabled,
  claimed,
  state,
  onRetry,
  onApply,
}: {
  enabled: boolean
  claimed: boolean
  state: SuggestionState
  onRetry: () => void
  onApply: (data: Suggestion) => void
}) {
  const data = state.status === "ready" ? state.data : null
  return (
    <section className="ai-card" aria-live="polite">
      <div className="ai-head">
        <Icon name="sparkle" size={16} />
        <strong>AI 审核建议</strong>
        <span>仅供参考，需人工确认</span>
      </div>
      {claimed ? (
        <p className="ai-status">
          <Icon name="check" size={15} />
          该案件已被认领，不再进行 AI 分析。
        </p>
      ) : !enabled ? (
        <p className="ai-status">
          后端未配置 AI 服务。请在 server/.env 中设置 AI_BASE_URL、AI_API_KEY 和
          AI_MODEL 后重启 Rust 服务。
        </p>
      ) : state.status === "error" ? (
        <>
          <p className="ai-status tone-red">
            <Icon name="alert" size={15} />
            {state.message}
          </p>
          <div className="ai-actions">
            <button
              type="button"
              className="btn btn-secondary"
              onClick={onRetry}
            >
              重试
            </button>
          </div>
        </>
      ) : data ? (
        <>
          <div className="ai-verdict">
            <strong className={ACTION_TONE[data.action]}>
              建议{actionLabel(data.action)}
            </strong>
            {data.violationType && <span>{data.violationType}</span>}
            {typeof data.confidence === "number" && (
              <span className="ai-confidence">
                置信度 {Math.round(data.confidence * 100)}%
              </span>
            )}
          </div>
          {data.reason && <p className="ai-reason">{data.reason}</p>}
          <div className="ai-actions">
            <button
              type="button"
              className="btn btn-secondary"
              onClick={() => onApply(data)}
            >
              填入表单
            </button>
            <button type="button" className="btn btn-plain" onClick={onRetry}>
              重新生成
            </button>
          </div>
        </>
      ) : (
        <p className="ai-status">
          <span className="spinner" aria-hidden="true" />
          正在分析评论与举报语境…
        </p>
      )}
    </section>
  )
}

function DetailSkeleton() {
  return (
    <div className="detail-content" aria-hidden="true">
      {[132, 96, 160].map((height) => (
        <div className="sk sk-card" key={height} style={{ height }} />
      ))}
    </div>
  )
}

function Guidelines() {
  const steps = [
    "输入 access-token，系统通过未读消息接口校验登录态。",
    "从待审核列表选择案件，查看评论、作品语境与全部举报记录。",
    "AI 辅助模式自动生成建议并填入表单；人工模式由审核员独立填写。",
    "确认受理结果、备注与站内信后提交，成功后案件归属当前审核员。",
  ]
  const rules = [
    "查看案件详情不会认领案件，只有提交成功后才会归属当前审核员。",
    "AI 仅提供建议，提交必须由审核员手动确认。",
    "一条评论可能被多人举报，提交时会受理全部举报记录。",
    "受理备注将公示于公告栏，请保持客观、简洁。",
    "提交失败（例如评论已被他人处理）时，列表会自动刷新。",
  ]
  return (
    <section className="guide">
      <div className="guide-inner">
        <header>
          <h1>审核规范</h1>
          <p>审核流程与关键规则。</p>
        </header>
        <Group title="审核流程">
          {steps.map((step, index) => (
            <div className="step" key={step}>
              <span className="step-index">{index + 1}</span>
              <p>{step}</p>
            </div>
          ))}
        </Group>
        <Group title="关键规则">
          {rules.map((rule) => (
            <div className="step" key={rule}>
              <span className="rule-icon">
                <Icon name="check" size={15} />
              </span>
              <p>{rule}</p>
            </div>
          ))}
        </Group>
      </div>
    </section>
  )
}

// ---- 作品审核模式（与评论审核数据完全独立） ----

type CompositionRecord = {
  caseId: number
  title: string
  nickname: string
  action: Action | 0
  at: number
  source: "local" | "synced"
}

const COMPOSITION_HISTORY_KEY = "qingfeng.composition.history"
const COMPOSITION_SYNCED_KEY = "qingfeng.composition.synced"
const COMPOSITION_HISTORY_LIMIT = 50
const COMPOSITION_SYNCED_TTL = 10 * 60 * 1000

function loadCompositionHistory(): CompositionRecord[] {
  try {
    const raw = localStorage.getItem(COMPOSITION_HISTORY_KEY)
    const parsed = raw ? JSON.parse(raw) : []
    return Array.isArray(parsed)
      ? parsed.filter(
          (item: CompositionRecord) =>
            item && typeof item.caseId === "number",
        )
      : []
  } catch {
    return []
  }
}

function saveCompositionHistory(list: CompositionRecord[]) {
  try {
    localStorage.setItem(
      COMPOSITION_HISTORY_KEY,
      JSON.stringify(list.slice(0, COMPOSITION_HISTORY_LIMIT)),
    )
  } catch {}
}

function loadCompositionSynced(owner?: string): {
  at: number
  records: CompositionRecord[]
} {
  try {
    const raw = localStorage.getItem(COMPOSITION_SYNCED_KEY)
    if (!raw) return { at: 0, records: [] }
    const parsed = JSON.parse(raw)
    if ((owner ?? "") !== String(parsed.owner ?? "")) {
      return { at: 0, records: [] }
    }
    const records = Array.isArray(parsed.records)
      ? parsed.records.filter(
          (item: CompositionRecord) =>
            item && typeof item.caseId === "number",
        )
      : []
    return { at: Number(parsed.at) || 0, records }
  } catch {
    return { at: 0, records: [] }
  }
}

function saveCompositionSynced(
  owner: string | undefined,
  at: number,
  records: CompositionRecord[],
) {
  try {
    localStorage.setItem(
      COMPOSITION_SYNCED_KEY,
      JSON.stringify({
        owner: owner ?? "",
        at,
        records: records.slice(0, COMPOSITION_HISTORY_LIMIT),
      }),
    )
  } catch {}
}

/** sourceUrl 为空时用 compositionId 拼社区作品地址（实测可用）。 */
function compositionLink(composition: {
  sourceUrl?: string
  compositionId?: string
}): string {
  const source = composition.sourceUrl?.trim()
  if (source) return source
  const id = composition.compositionId?.trim()
  return id
    ? `https://world.xiaomawang.com/community/main/compose/${id}`
    : ""
}

function compositionWhen(unix?: number) {
  if (!unix || unix <= 0) return ""
  return new Date(unix * 1000).toLocaleString("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  })
}

function compositionStale(item: CaseSummary) {
  const timestamp = item.lastReportTime ?? 0
  return timestamp > 0 && Date.now() - timestamp * 1000 > 7 * 24 * 60 * 60 * 1000
}

function CompositionApp({
  api,
  session,
  demo,
  active,
  reviewerId,
  onReviewerIdChange,
  onModeChange,
  onLogout,
}: {
  api: ReviewApi
  session: Session
  demo: boolean
  /** 当前显示的是不是作品模式：隐藏时不跑后台轮询，但保留已加载的数据。 */
  active: boolean
  reviewerId: number
  onReviewerIdChange: (id: number) => void
  /** 切换评论 / 作品审核模式。 */
  onModeChange: (mode: ReviewMode) => void
  onLogout: (message?: string) => void
}) {
  // 开源版没有账号：缓存按 token 掩码分区，换 token 不会串到上一个审核员的数据。
  const owner = session.community?.masked ?? ""
  // 窄屏（手机/平板）侧边栏是抽屉：默认收起，由左下角按钮拉开。
  const [drawerOpen, setDrawerOpen] = useState(false)
  const [view, setView] = useState<"cases" | "history" | "guide">("cases")
  const [page, setPage] = useState(1)
  const [hiddenPage, setHiddenPage] = useState(1)
  const [pageSize, setPageSize] = useState(10)
  const [data, setData] = useState<CasePage | null>(null)
  const [loading, setLoading] = useState(true)
  const [listError, setListError] = useState("")
  const [reloadKey, setReloadKey] = useState(0)
  const [query, setQuery] = useState("")
  const [searchId, setSearchId] = useState<number | null>(null)
  const [showHidden, setShowHidden] = useState(false)
  const [selectedId, setSelectedId] = useState<number | null>(null)
  const [history, setHistory] =
    useState<CompositionRecord[]>(loadCompositionHistory)
  const [synced, setSynced] = useState<CompositionRecord[]>(
    () => loadCompositionSynced(owner).records,
  )
  const [syncedAt, setSyncedAt] = useState(
    () => loadCompositionSynced(owner).at,
  )
  const [syncing, setSyncing] = useState(false)
  const [settingsOpen, setSettingsOpen] = useState(false)
  const [toast, setToast] = useState<{ text: string; tone: Tone } | null>(null)
  const autoSelected = useRef(false)
  const syncedTryAt = useRef(0)
  const tokenExpired = !demo && !session.community?.hasToken

  const notify = useCallback((text: string, tone: Tone = "success") => {
    setToast({ text, tone })
    window.setTimeout(() => setToast(null), 2800)
  }, [])
  const fail = useCallback(
    (error: unknown) => notify(messageOf(error), "error"),
    [notify],
  )

  // 已经取过的分页组合：切回作品模式时复用内存里的列表，不重复扫全表。
  const loadedKey = useRef("")
  const listController = useRef<AbortController | null>(null)

  useEffect(() => {
    // 切回评论模式时不刷新作品列表：子树还挂着，数据和选中项都留着。
    if (!active) return
    const key = `${page}|${pageSize}|${hiddenPage}|${searchId ?? ""}|${reloadKey}`
    if (loadedKey.current === key) return
    loadedKey.current = key
    listController.current?.abort()
    const controller = new AbortController()
    listController.current = controller
    setLoading(true)
    setListError("")
    api
      .compositionCases(page, pageSize, {
        reportId: searchId ?? undefined,
        hiddenPage,
        signal: controller.signal,
      })
      .then((result) => {
        if (controller.signal.aborted) return
        setData(result)
        if (
          result.search &&
          (result.hiddenList?.length ?? 0) > 0 &&
          result.list.length === 0
        ) {
          setShowHidden(true)
        }
        const first =
          result.list[0] ??
          (result.search ? result.hiddenList?.[0] : undefined)
        if (!autoSelected.current && first) {
          setSelectedId(first.id)
          autoSelected.current = true
        }
      })
      .catch((error) => {
        if (controller.signal.aborted) return
        loadedKey.current = ""
        if (isExpired(error)) onLogout(EXPIRED_MESSAGE)
        else setListError(messageOf(error))
      })
      .finally(() => {
        if (!controller.signal.aborted) setLoading(false)
      })
  }, [
    active,
    api,
    page,
    pageSize,
    searchId,
    hiddenPage,
    reloadKey,
    onLogout,
  ])
  // 退出登录卸载子树时才掐断在飞的作品列表请求。
  useEffect(
    () => () => {
      listController.current?.abort()
    },
    [],
  )

  const normalCases = useMemo(() => data?.list ?? [], [data])
  const hiddenCases = useMemo(() => data?.hiddenList ?? [], [data])
  const allCases = useMemo(() => {
    const seen = new Map<number, CaseSummary>()
    for (const item of [...normalCases, ...hiddenCases])
      seen.set(item.id, item)
    return [...seen.values()]
  }, [normalCases, hiddenCases])
  // 详情页的只读状态以列表为准；列表里没有该案件时为 null，退回详情字段。
  const selectedClaimed = useMemo(() => {
    if (selectedId === null) return null
    const summary = allCases.find((item) => item.id === selectedId)
    return summary ? !unclaimed(summary.checkerName) : null
  }, [allCases, selectedId])
  const normalTotal = data?.normalTotal ?? data?.total ?? 0
  const hiddenTotal = data?.hiddenTotal ?? 0
  const hiddenClaimed = data?.hiddenClaimedTotal ?? 0
  const hiddenStale = data?.hiddenStaleTotal ?? 0
  const keyword = searchId !== null ? "" : query.trim().toLowerCase()
  const matches = useCallback(
    (item: CaseSummary) =>
      !keyword ||
      [item.title, item.nickname, String(item.id), item.compositionId].some(
        (value) => (value ?? "").toLowerCase().includes(keyword),
      ),
    [keyword],
  )
  const filteredNormal = useMemo(
    () => normalCases.filter(matches),
    [normalCases, matches],
  )
  const filteredHidden = useMemo(
    () => hiddenCases.filter(matches),
    [hiddenCases, matches],
  )
  const totalPages = data?.search
    ? 1
    : Math.max(1, Math.ceil(normalTotal / pageSize))
  const hiddenTotalPages = data?.search
    ? 1
    : Math.max(1, Math.ceil(hiddenTotal / pageSize))

  useEffect(() => {
    if (data && page > totalPages) setPage(totalPages)
  }, [data, page, totalPages])
  useEffect(() => {
    if (data && hiddenPage > hiddenTotalPages)
      setHiddenPage(hiddenTotalPages)
  }, [data, hiddenPage, hiddenTotalPages])

  const historyEntries = useMemo(() => {
    const merged = new Map<number, CompositionRecord>()
    for (const item of history) merged.set(item.caseId, item)
    for (const item of synced) {
      if (!merged.has(item.caseId)) merged.set(item.caseId, item)
    }
    return [...merged.values()]
      .sort((a, b) => b.at - a.at)
      .slice(0, COMPOSITION_HISTORY_LIMIT)
  }, [history, synced])

  const syncHistory = useCallback(
    async (force = false) => {
      if (syncing) return
      if (!force && Date.now() - syncedAt < COMPOSITION_SYNCED_TTL) return
      if (!force && Date.now() - syncedTryAt.current < 60_000) return
      if (reviewerId <= 0) {
        if (force)
          notify("请先在设置里填写审核员 ID（社区 checkerId）", "error")
        return
      }
      syncedTryAt.current = Date.now()
      setSyncing(true)
      try {
        const result = await api.compositionSyncedHistory({
          reviewerId,
          limit: COMPOSITION_HISTORY_LIMIT,
          pages: 40,
        })
        const at = Date.now()
        const entries: CompositionRecord[] = result.records.map((item) => ({
          caseId: item.caseId,
          title: item.title,
          nickname: item.nickname,
          action: 0,
          at: item.lastReportTime > 0 ? item.lastReportTime * 1000 : at,
          source: "synced",
        }))
        setSynced(entries)
        setSyncedAt(at)
        saveCompositionSynced(owner, at, entries)
        if (force) {
          notify(
            entries.length > 0
              ? `已从作品列表同步 ${entries.length} 条历史`
              : "最近作品里没有找到你的审核记录",
            entries.length > 0 ? "success" : "error",
          )
        }
      } catch (error) {
        if (force) notify(`作品历史同步失败：${messageOf(error)}`, "error")
        else
          console.warn("[composition] 作品历史同步失败：", messageOf(error))
      } finally {
        setSyncing(false)
      }
    },
    [
      api,
      notify,
      reviewerId,
      owner,
      syncedAt,
      syncing,
    ],
  )

  useEffect(() => {
    if (active && view === "history") void syncHistory()
  }, [active, view, syncHistory])

  function runSearch() {
    const value = query.trim()
    if (!/^\d{1,18}$/.test(value) || Number(value) <= 0) return
    setSearchId(Number(value))
    setSelectedId(null)
    autoSelected.current = false
    setPage(1)
    setHiddenPage(1)
  }

  function clearSearch() {
    setQuery("")
    setSearchId(null)
    setPage(1)
    setHiddenPage(1)
  }

  function handleSubmitted(info: {
    caseId: number
    title: string
    nickname: string
    action: Action
  }) {
    const record: CompositionRecord = {
      ...info,
      at: Date.now(),
      source: "local",
    }
    setHistory((current) => {
      const next = [
        record,
        ...current.filter((item) => item.caseId !== record.caseId),
      ].slice(0, COMPOSITION_HISTORY_LIMIT)
      saveCompositionHistory(next)
      return next
    })
    notify(`已提交作品审核 · 案件 #${info.caseId}`)
    setSelectedId(null)
    setReloadKey((value) => value + 1)
  }

  function removeHistory(record: CompositionRecord) {
    if (record.source !== "local") return
    setHistory((current) => {
      const next = current.filter((item) => item.caseId !== record.caseId)
      saveCompositionHistory(next)
      return next
    })
    notify(`已删除本机作品历史 #${record.caseId}`)
  }

  return (
    <div
      className={`console ${selectedId !== null ? "has-selection" : ""}${
        drawerOpen ? " drawer-open" : ""
      }`}
    >
      <div
        className="sidebar-scrim"
        aria-hidden="true"
        onClick={() => setDrawerOpen(false)}
      />
      <button
        type="button"
        className="sidebar-toggle"
        aria-label="展开侧边栏"
        aria-expanded={drawerOpen}
        onClick={() => setDrawerOpen(true)}
      >
        <Icon name="menu" size={20} />
      </button>
      <aside className="sidebar">
        <div className="sidebar-brand">
          <AppMark />
          <div>
            <strong>作品审核</strong>
            <span>Scratch 作品安全</span>
          </div>
        </div>
        <nav
          className="sidebar-nav"
          aria-label="作品审核导航"
          onClick={() => setDrawerOpen(false)}
        >
          <p className="sidebar-caption">审核</p>
          <button
            className={`side-item ${view === "cases" ? "active" : ""}`}
            onClick={() => setView("cases")}
          >
            <Icon name="book" size={17} />
            <span className="side-label">作品待审</span>
            {data && <span className="side-count">{normalTotal}</span>}
          </button>
          <button
            className={`side-item ${view === "history" ? "active" : ""}`}
            onClick={() => setView("history")}
          >
            <Icon name="clock" size={17} />
            <span className="side-label">作品历史</span>
            {historyEntries.length > 0 && (
              <span className="side-count">{historyEntries.length}</span>
            )}
          </button>
          <button
            className={`side-item ${view === "guide" ? "active" : ""}`}
            onClick={() => setView("guide")}
          >
            <Icon name="shield" size={17} />
            <span className="side-label">审核说明</span>
          </button>
          <p className="sidebar-caption">设置</p>
          <button className="side-item" onClick={() => setSettingsOpen(true)}>
            <Icon name="gear" size={17} />
            <span className="side-label">作品审核设置</span>
          </button>
        </nav>
        <ModeSwitch
          mode={active ? "composition" : "comment"}
          onSwitch={(next) => {
            setDrawerOpen(false)
            onModeChange(next)
          }}
        />
        <div className="sidebar-account">
          <Avatar name="审" size="sm" />
          <div>
            <strong>{demo ? "示例数据" : "社区审核员"}</strong>
            <span
              title={
                session.community?.masked
                  ? `凭证 ${session.community.masked}`
                  : undefined
              }
            >
              {demo
                ? "示例数据，不会提交"
                : tokenExpired
                  ? "尚未获取社区凭证"
                  : session.community?.masked
                    ? `凭证 ${session.community.masked}`
                    : "访客模式"}
            </span>
          </div>
          <button
            className="icon-button"
            aria-label="退出登录"
            title="退出登录"
            onClick={() => onLogout()}
          >
            <Icon name="logout" size={17} />
          </button>
        </div>
      </aside>

      {view === "guide" ? (
        <CompositionGuidelines />
      ) : view === "history" ? (
        <section className="history-pane compose-wide" aria-label="作品历史">
          <header className="list-header">
            <div className="list-title">
              <h1>作品历史</h1>
              <span>
                已审核作品按审核员同步 · 作品审核不统计满意度
                {syncing
                  ? " · 正在从作品列表同步…"
                  : syncedAt
                    ? ` · 上次同步 ${new Date(syncedAt).toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit" })}`
                    : ""}
              </span>
            </div>
            <div className="list-actions">
              <button
                className="icon-button"
                aria-label="从作品列表同步历史"
                title="从作品列表同步历史"
                onClick={() => void syncHistory(true)}
                disabled={syncing}
              >
                <Icon name="tray" size={17} />
              </button>
              <button
                className="icon-button"
                aria-label="清空本机作品历史"
                title="清空本机作品历史"
                onClick={() => {
                  setHistory(() => {
                    saveCompositionHistory([])
                    return []
                  })
                }}
                disabled={history.length === 0}
              >
                <Icon name="close" size={17} />
              </button>
            </div>
          </header>
          <div className="history-list">
            {historyEntries.length === 0 ? (
              <EmptyState
                icon="book"
                title="还没有作品审核记录"
                text={
                  reviewerId > 0
                    ? "提交作品审核后会记在这里；也可以点右上角从作品列表同步。"
                    : "先在设置里填写审核员 ID，再从作品列表同步历史。"
                }
              />
            ) : (
              historyEntries.map((record) => (
                <CompositionHistoryRow
                  key={`${record.caseId}-${record.at}`}
                  record={record}
                  onOpen={() => {
                    setSelectedId(record.caseId)
                    setView("cases")
                  }}
                  onDelete={() => removeHistory(record)}
                />
              ))
            )}
          </div>
        </section>
      ) : (
        <>
          <section className="list-pane" aria-label="作品待审">
            {loading && data && <span className="refresh-bar" aria-hidden="true" />}
            <header className="list-header">
              <div className="list-title">
                <h1>作品待审</h1>
                <span>
                  {data
                    ? searchId !== null
                      ? `作品 #${searchId} 搜索结果`
                      : `${normalTotal} 个待审核作品`
                    : "正在加载"}
                  {hiddenTotal > 0
                    ? ` · 已隐藏 ${[
                        hiddenClaimed ? `${hiddenClaimed} 件已认领` : "",
                        hiddenStale ? `${hiddenStale} 件超期` : "",
                      ]
                        .filter(Boolean)
                        .join("、")}`
                    : ""}
                </span>
              </div>
              <div className="list-actions">
                <button
                  className="icon-button"
                  aria-label="刷新"
                  title="刷新作品列表"
                  onClick={() => setReloadKey((value) => value + 1)}
                >
                  <Icon name="refresh" size={17} />
                </button>
              </div>
            </header>
            <form
              className="search-field"
              onSubmit={(event) => {
                event.preventDefault()
                runSearch()
              }}
            >
              <Icon name="search" size={15} />
              <input
                type="search"
                aria-label="搜索作品编号或当前页内容"
                placeholder="作品编号 / 标题 / 作者"
                value={query}
                onChange={(event) => {
                  setQuery(event.target.value)
                  if (searchId !== null) setSearchId(null)
                }}
              />
              {(query || searchId !== null) && (
                <button
                  type="button"
                  className="search-clear"
                  aria-label="清除搜索"
                  onClick={clearSearch}
                >
                  <Icon name="close" size={13} />
                </button>
              )}
            </form>
            {searchId !== null && (
              <div className="search-result-bar" role="status">
                <span>
                  作品 #{searchId}：共{" "}
                  {(data?.list.length ?? 0) +
                    (data?.hiddenList?.length ?? 0)}{" "}
                  条记录
                </span>
                <button
                  type="button"
                  className="link-pill"
                  onClick={clearSearch}
                >
                  返回列表
                </button>
              </div>
            )}
            {hiddenTotal > 0 && (
              <button
                type="button"
                className={`hidden-toggle ${showHidden ? "on" : ""}`}
                aria-pressed={showHidden}
                onClick={() => setShowHidden((value) => !value)}
              >
                <Icon name={showHidden ? "close" : "tray"} size={13} />
                {showHidden
                  ? "收起已隐藏作品"
                  : `展开已隐藏作品（${[
                      hiddenClaimed ? `${hiddenClaimed} 件已认领` : "",
                      hiddenStale ? `${hiddenStale} 件超期` : "",
                    ]
                      .filter(Boolean)
                      .join(" · ")}）`}
              </button>
            )}
            <div className="case-list" aria-busy={loading}>
              {listError ? (
                <EmptyState
                  icon="alert"
                  title="无法加载作品"
                  text={listError}
                  action={
                    <button
                      className="btn btn-secondary"
                      onClick={() => setReloadKey((value) => value + 1)}
                    >
                      重试
                    </button>
                  }
                />
              ) : !data ? (
                <ListSkeleton />
              ) : (
                <>
                  {showHidden && hiddenTotal > 0 && (
                    <div className="hidden-group leading">
                      <p className="hidden-group-title">
                        已隐藏 {hiddenTotal} 件（
                        {[
                          hiddenClaimed
                            ? `${hiddenClaimed} 件已认领`
                            : "",
                          hiddenStale ? `${hiddenStale} 件超期` : "",
                        ]
                          .filter(Boolean)
                          .join("、")}
                        ）
                      </p>
                      {filteredHidden.length === 0 ? (
                        <p className="hidden-empty">
                          本页没有隐藏作品。
                        </p>
                      ) : (
                        filteredHidden.map((item) => (
                          <CompositionRow
                            key={item.id}
                            item={item}
                            active={item.id === selectedId}
                            onSelect={() => setSelectedId(item.id)}
                          />
                        ))
                      )}
                      {data?.search !== true && (
                        <div className="hidden-footer">
                          <span>
                            第 {hiddenPage} / {hiddenTotalPages} 页
                          </span>
                          <div className="pager">
                            <button
                              className="icon-button"
                              aria-label="隐藏作品上一页"
                              disabled={hiddenPage <= 1 || loading}
                              onClick={() =>
                                setHiddenPage((value) => value - 1)
                              }
                            >
                              <Icon name="prev" size={16} />
                            </button>
                            <button
                              className="icon-button"
                              aria-label="隐藏作品下一页"
                              disabled={
                                hiddenPage >= hiddenTotalPages || loading
                              }
                              onClick={() =>
                                setHiddenPage((value) => value + 1)
                              }
                            >
                              <Icon name="next" size={16} />
                            </button>
                          </div>
                        </div>
                      )}
                    </div>
                  )}
                  {filteredNormal.length === 0 ? (
                    <EmptyState
                      icon={
                        searchId !== null
                          ? "search"
                          : hiddenTotal > 0 && !query
                            ? "alert"
                            : "check"
                      }
                      title={
                        searchId !== null
                          ? "该作品已隐藏或不存在"
                          : query
                            ? "没有匹配的作品"
                            : hiddenTotal > 0
                              ? "本页没有可审理的作品"
                              : "暂无待审核作品"
                      }
                      text={
                        hiddenTotal > 0
                          ? "已认领或超过一周的作品会自动折叠，可展开查看。"
                          : "所有作品举报都已处理完毕。"
                      }
                    />
                  ) : (
                    filteredNormal.map((item) => (
                      <CompositionRow
                        key={item.id}
                        item={item}
                        active={item.id === selectedId}
                        onSelect={() => setSelectedId(item.id)}
                      />
                    ))
                  )}
                </>
              )}
            </div>
            <footer className="list-footer">
              <select
                aria-label="每页条数"
                value={pageSize}
                onChange={(event) => {
                  setPageSize(Number(event.target.value))
                  setPage(1)
                  setHiddenPage(1)
                }}
              >
                {[10, 20, 30, 40].map((size) => (
                  <option key={size} value={size}>
                    {size} 条/页
                  </option>
                ))}
              </select>
              <div className="pager">
                <button
                  className="icon-button"
                  aria-label="上一页"
                  disabled={page <= 1 || loading}
                  onClick={() => setPage((value) => value - 1)}
                >
                  <Icon name="prev" size={16} />
                </button>
                <span>
                  {page} / {totalPages}
                </span>
                <button
                  className="icon-button"
                  aria-label="下一页"
                  disabled={page >= totalPages || loading}
                  onClick={() => setPage((value) => value + 1)}
                >
                  <Icon name="next" size={16} />
                </button>
              </div>
            </footer>
          </section>
          <section className="detail-pane" aria-label="作品详情">
            {selectedId !== null ? (
              <CompositionReviewPane
                api={api}
                caseId={selectedId}
                demo={demo}
                claimed={selectedClaimed}
                onBack={() => setSelectedId(null)}
                onSubmitted={handleSubmitted}
                onError={fail}
              />
            ) : (
              <EmptyState
                icon="book"
                title="选择一个作品开始审核"
                text="作品会内嵌在审核信息里；AI 不支持 Scratch 作品，作品审核仅人工处理。"
              />
            )}
          </section>
        </>
      )}

      {settingsOpen && (
        <CompositionSettingsDialog
          reviewerId={reviewerId}
          tokenMasked={
            session.community?.hasToken ? session.community.masked : ""
          }
          onSave={(id) => {
            onReviewerIdChange(id)
            setSettingsOpen(false)
            notify(id > 0 ? `审核员 ID 已保存：${id}` : "审核员 ID 已清空")
          }}
          onClose={() => setSettingsOpen(false)}
        />
      )}

      {toast && (
        <div className={`toast toast-${toast.tone}`} role="status">
          <Icon name={toast.tone === "error" ? "alert" : "check"} size={15} />
          {toast.text}
        </div>
      )}
    </div>
  )
}

function CompositionRow({
  item,
  active,
  onSelect,
}: {
  item: CaseSummary
  active: boolean
  onSelect: () => void
}) {
  const stale = compositionStale(item)
  return (
    <div
      className={`case-row ${active ? "active" : ""}`}
      role="button"
      tabIndex={0}
      aria-current={active || undefined}
      onClick={onSelect}
      onKeyDown={(event) => {
        if (event.key !== "Enter" && event.key !== " ") return
        event.preventDefault()
        onSelect()
      }}
    >
      {/* 列表不取封面：封面只有进详情才拿得到，这里的方块只用来对齐分隔线缩进。 */}
      <span className="composition-mark" aria-hidden="true">
        <Icon name="book" size={18} />
      </span>
      <div className="case-row-body">
        <div className="case-row-top">
          <strong>{item.title || "未命名作品"}</strong>
          <span>#{item.id}</span>
        </div>
        <span className="case-row-content">
          {item.nickname ? `作者 ${item.nickname}` : "未知作者"}
          {item.reportNum ? ` · ${item.reportNum} 次举报` : ""}
        </span>
        <span className="case-row-meta">
          <span>{compositionWhen(item.lastReportTime)}</span>
          {unclaimed(item.checkerName) ? (
            <span className="case-badge case-badge-open">
              <Icon name="tray" size={11} />
              尚未认领
            </span>
          ) : (
            <span className="case-badge case-badge-open">已认领</span>
          )}
          {stale && (
            <span className="case-badge case-badge-stale">
              <Icon name="alert" size={11} />
              超过一周
            </span>
          )}
        </span>
      </div>
    </div>
  )
}

function CompositionReviewPane({
  api,
  caseId,
  demo,
  claimed,
  onBack,
  onSubmitted,
  onError,
}: {
  api: ReviewApi
  caseId: number
  demo: boolean
  /** 列表给出的认领状态；null = 列表里没有该案件，退回详情字段判断。 */
  claimed: boolean | null
  onBack: () => void
  onSubmitted: (info: {
    caseId: number
    title: string
    nickname: string
    action: Action
  }) => void
  onError: (error: unknown) => void
}) {
  const [detail, setDetail] = useState<CaseDetail | null>(null)
  const [loadError, setLoadError] = useState("")
  const [form, setForm] = useState<FormState>(EMPTY_FORM)
  const [formError, setFormError] = useState("")
  const [submitting, setSubmitting] = useState(false)
  const alive = useRef(true)

  useEffect(() => {
    alive.current = true
    return () => {
      alive.current = false
    }
  }, [])

  const loadDetail = useCallback(() => {
    setLoadError("")
    setDetail(null)
    setForm(EMPTY_FORM)
    setFormError("")
    api
      .compositionDetail(caseId)
      .then((result) => {
        if (!alive.current) return
        setDetail(result)
      })
      .catch((error) => {
        if (!alive.current) return
        if (isExpired(error)) onError(error)
        else setLoadError(messageOf(error))
      })
  }, [api, caseId, onError])

  useEffect(loadDetail, [loadDetail])

  const age = detail ? caseAgeOf(detail) : null
  const stale = detail ? age === null || isStale(age) : false
  // 与评论一致：以列表状态为准，避免多举报案件被详情里的旧 checkerId 锁死。
  const claimedByDetail = Boolean(detail?.isDeal) || Boolean(detail?.checkerId)
  const claimedNow = claimed === null ? claimedByDetail : claimed
  const readOnly = claimedNow || stale
  const link = detail ? compositionLink(detail.composition) : ""

  function chooseAction(action: Action) {
    if (readOnly) return
    setFormError("")
    const template =
      COMPOSITION_TEMPLATES[action as 2 | 3] ?? COMPOSITION_TEMPLATES[3]
    setForm((current) => ({
      ...current,
      action,
      userMessage: isTemplate(current.userMessage, "user")
        ? template.user
        : current.userMessage,
      reporterMessage: isTemplate(current.reporterMessage, "reporter")
        ? template.reporter
        : current.reporterMessage,
    }))
  }

  function update(patch: Partial<FormState>) {
    if (readOnly) return
    setFormError("")
    setForm((current) => ({ ...current, ...patch }))
  }

  async function submit(event: FormEvent) {
    event.preventDefault()
    if (submitting || !detail || readOnly) return
    if (!form.action) return setFormError("请选择受理结果")
    if (!form.remark.trim() || !form.reporterMessage.trim()) {
      return setFormError("请填写受理备注和举报人站内信")
    }
    // 下架必须给作者发站内信；正常显示不需要，也不发送这个字段。
    if (form.action === 2 && !form.userMessage.trim()) {
      return setFormError("下架需要填写被举报人站内信")
    }
    setSubmitting(true)
    try {
      await api.verifyComposition(detail.reportId, {
        action: form.action,
        remark: form.remark.trim(),
        userMessage: form.action === 2 ? form.userMessage.trim() : "",
        reporterMessage: form.reporterMessage.trim(),
      })
      onSubmitted({
        caseId: detail.reportId,
        title: detail.composition.title || `作品 #${detail.reportId}`,
        nickname: detail.author.nickname,
        action: form.action,
      })
    } catch (error) {
      if (alive.current) setSubmitting(false)
      if (isExpired(error)) onError(error)
      else setFormError(messageOf(error))
    }
  }

  return (
    <div className="review">
      <header className="detail-toolbar">
        <button
          className="icon-button mobile-only"
          aria-label="返回列表"
          onClick={onBack}
        >
          <Icon name="back" size={18} />
        </button>
        <div className="detail-title">
          <strong>作品 #{caseId}</strong>
          <span>
            {detail
              ? `${detail.reports.length} 条举报 · ${
                  detail.isDeal ? "已处理" : "待处理"
                }`
              : "正在加载"}
          </span>
        </div>
      </header>
      <div className="detail-scroll">
        {loadError ? (
          <EmptyState
            icon="alert"
            title="无法加载作品详情"
            text={loadError}
            action={
              <button className="btn btn-secondary" onClick={loadDetail}>
                重试
              </button>
            }
          />
        ) : !detail ? (
          <DetailSkeleton />
        ) : (
          <div className="detail-content">
            <Group
              title="作品预览"
              aside={
                link && (
                  <ExternalLink href={link}>在新窗口打开</ExternalLink>
                )
              }
            >
              <div className="composition-heading">
                {/* 封面只在详情里出现：详情页的 composition 才带可靠的 coverUrl。 */}
                {detail.composition.coverUrl && (
                  <span className="composition-cover">
                    <img src={detail.composition.coverUrl} alt="" loading="lazy" />
                  </span>
                )}
                <div className="composition-heading-text">
                  <strong>{detail.composition.title || "未命名作品"}</strong>
                  <span>
                    {detail.composition.compositionId}
                    {detail.composition.publishTime
                      ? ` · ${detail.composition.publishTime}`
                      : ""}
                  </span>
                </div>
              </div>
              {link ? (
                <iframe
                  className="composition-frame"
                  src={link}
                  title={detail.composition.title || "作品预览"}
                  allow="fullscreen; autoplay"
                />
              ) : (
                <p className="muted-block">未获取到作品网页地址。</p>
              )}
              <div className="info-row">
                <span>作者</span>
                <strong>{detail.author.nickname || "未知用户"}</strong>
              </div>
              <div className="info-row">
                <span>数据</span>
                <strong>
                  浏览 {detail.composition.viewCount} · 点赞{" "}
                  {detail.composition.likeCount} · 评论{" "}
                  {detail.composition.commentCount} · 收藏{" "}
                  {detail.composition.collectCount}
                </strong>
              </div>
              {detail.composition.tags.length > 0 && (
                <div className="tags">
                  {detail.composition.tags.map((tag) => (
                    <span className="tag" key={tag}>
                      {tag}
                    </span>
                  ))}
                </div>
              )}
              {detail.composition.description && (
                <p className="muted-block">
                  {detail.composition.description}
                </p>
              )}
            </Group>

            <Group
              title="举报记录"
              aside={
                <span className="group-aside">
                  共 {detail.reportNum} 次 · 提交时全部受理
                </span>
              }
            >
              {detail.reports.map((report) => (
                <div className="report" key={report.id}>
                  <Avatar
                    src={report.reporter.avatar}
                    name={report.reporter.nickname || "用"}
                    size="sm"
                  />
                  <div className="report-body">
                    <div className="report-top">
                      <strong>
                        {report.reporter.nickname || "匿名用户"}
                      </strong>
                      <span>{report.createdAt}</span>
                    </div>
                    <span className="badge">
                      {report.reasonName || "未分类"}
                    </span>
                    <p className="report-text">
                      {report.content || "未填写举报描述"}
                    </p>
                    {report.images.length > 0 && (
                      <div className="report-images">
                        {report.images.map((src) => (
                          <a
                            key={src}
                            href={src}
                            target="_blank"
                            rel="noreferrer"
                          >
                            <img
                              src={src}
                              alt="举报凭证"
                              loading="lazy"
                            />
                          </a>
                        ))}
                      </div>
                    )}
                  </div>
                  <span className="status-pill">未处理</span>
                </div>
              ))}
            </Group>

            <form className="review-form" onSubmit={submit}>
              <Group title="受理">
                {readOnly && (
                  <p className="readonly-note" role="status">
                    <Icon name={stale ? "clock" : "check"} size={15} />
                    {stale
                      ? "该作品举报已超过一周或时间无法确认，系统不再提交。"
                      : "该作品案件已被认领，仅可查看。"}
                  </p>
                )}
                <div className="form-row">
                  <span className="form-label">受理结果</span>
                  <Segmented
                    label="受理结果"
                    value={form.action}
                    onChange={chooseAction}
                    options={COMPOSITION_ACTIONS}
                    disabled={readOnly}
                  />
                </div>
                <TextArea
                  label="受理备注"
                  hint="将公示于公告栏"
                  rows={2}
                  value={form.remark}
                  placeholder="例如：作品含违规内容"
                  disabled={readOnly}
                  onChange={(remark) => update({ remark })}
                />
                {form.action === 2 && (
                  <TextArea
                    label="被举报人站内信"
                    hint="下架时发送给作品作者"
                    rows={3}
                    value={form.userMessage}
                    placeholder="例如：您的作品存在违规内容，已被下架…"
                    disabled={readOnly}
                    onChange={(userMessage) => update({ userMessage })}
                  />
                )}
                <TextArea
                  label="举报人站内信"
                  value={form.reporterMessage}
                  placeholder="选择受理结果后自动填入默认文案"
                  disabled={readOnly}
                  onChange={(reporterMessage) =>
                    update({ reporterMessage })
                  }
                />
              </Group>
              {formError && (
                <p className="form-error" role="alert">
                  <Icon name="alert" size={15} />
                  {formError}
                </p>
              )}
              <div className="submit-bar">
                <span>
                  <Icon name="shield" size={15} />
                  {readOnly
                    ? "当前作品只可查看"
                    : demo
                      ? "示例模式不会提交到社区"
                      : "提交后案件将归属当前审核员"}
                </span>
                <button
                  className="btn btn-primary btn-large"
                  disabled={submitting || readOnly}
                >
                  {readOnly
                    ? "不可提交"
                    : submitting
                      ? "正在提交…"
                      : "提交作品审核"}
                </button>
              </div>
            </form>
          </div>
        )}
      </div>
    </div>
  )
}

function CompositionHistoryRow({
  record,
  onOpen,
  onDelete,
}: {
  record: CompositionRecord
  onOpen: () => void
  onDelete: () => void
}) {
  const when = new Date(record.at).toLocaleString("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  })
  return (
    <div
      className="history-row"
      role="button"
      tabIndex={0}
      title="查看作品详情"
      onClick={onOpen}
      onKeyDown={(event) => {
        if (event.key !== "Enter" && event.key !== " ") return
        event.preventDefault()
        onOpen()
      }}
    >
      <div className="history-top">
        <strong>{record.title || "未命名作品"}</strong>
        <span className="history-id">#{record.caseId}</span>
        {record.action && (
          <span className={`case-badge ${compositionTone(record.action)}`}>
            {compositionActionLabel(record.action)}
          </span>
        )}
        {record.source === "synced" && (
          <span className="case-badge case-badge-open">同步</span>
        )}
        <span className="history-time">{when}</span>
        {record.source === "local" && (
          <button
            type="button"
            className="icon-button history-delete"
            aria-label={`删除作品 #${record.caseId} 的记录`}
            title="删除这条作品历史"
            onClick={(event) => {
              event.stopPropagation()
              onDelete()
            }}
          >
            <Icon name="trash" size={15} />
          </button>
        )}
      </div>
      <p className="history-content">
        {record.nickname ? `作者 ${record.nickname}` : "作者未知"}
      </p>
      <div className="history-meta">
        <span className="history-hint">查看详情</span>
        {record.source === "synced" && (
          <span className="history-report-time">已审核作品列表</span>
        )}
      </div>
    </div>
  )
}

function CompositionSettingsDialog({
  reviewerId,
  tokenMasked,
  onSave,
  onClose,
}: {
  reviewerId: number
  tokenMasked: string
  onSave: (id: number) => void
  onClose: () => void
}) {
  const [text, setText] = useState(reviewerId > 0 ? String(reviewerId) : "")
  function save() {
    const value = Number(text.trim())
    onSave(Number.isFinite(value) && value > 0 ? Math.floor(value) : 0)
  }
  return (
    <div className="modal-backdrop" role="presentation" onClick={onClose}>
      <div
        className="modal"
        role="dialog"
        aria-modal="true"
        aria-label="作品审核设置"
        onClick={(event) => event.stopPropagation()}
      >
        <header className="modal-head">
          <h2>作品审核设置</h2>
          <button className="icon-button" aria-label="关闭" onClick={onClose}>
            <Icon name="close" size={16} />
          </button>
        </header>
        <div className="modal-body">
          <div className="setting-block">
            <h3>审核员</h3>
            <p className="setting-line">
              <Icon name="user" size={13} />
              <span>社区审核员</span>
              <em>
                {tokenMasked
                  ? `社区凭证 ${tokenMasked}`
                  : "尚未获取社区凭证"}
              </em>
            </p>
            <label className="setting-row">
              <span>
                <strong>审核员 ID</strong>
                <em>
                  社区 checkerId；已审核作品按它同步，评论/作品共用
                </em>
              </span>
              <input
                className="setting-input"
                inputMode="numeric"
                placeholder="例如 3369552"
                value={text}
                onChange={(event) => setText(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    event.preventDefault()
                    save()
                  }
                }}
              />
            </label>
          </div>
          <p className="muted-block">
            社区 access-token 在评论审核的设置里维护；作品审核不抓取满意度。
          </p>
        </div>
        <footer className="modal-foot">
          <p />
          <div className="modal-foot-right">
            <button className="btn btn-secondary" onClick={onClose}>
              取消
            </button>
            <button className="btn btn-primary" onClick={save}>
              保存
            </button>
          </div>
        </footer>
      </div>
    </div>
  )
}

function CompositionGuidelines() {
  const steps = [
    "打开作品待审，选择一条作品举报。",
    "在审核信息中直接预览作品网页，阅读举报原因与举报记录。",
    "选择受理结果、填写受理备注和两条站内信后提交。",
  ]
  const rules = [
    "作品审核仅人工处理：AI 不支持 Scratch 作品，不会生成建议。",
    "被认领、超过一周或举报时间无法确认的作品会自动折叠。",
    "作品审核不抓取满意度；历史由已审核作品列表按审核员同步。",
    "评论审核与作品审核的数据、历史、分页完全独立。",
  ]
  return (
    <section className="guide">
      <div className="guide-inner">
        <header>
          <h1>作品审核规范</h1>
          <p>作品举报的处理流程与关键规则。</p>
        </header>
        <Group title="审核流程">
          {steps.map((step, index) => (
            <div className="step" key={step}>
              <span className="step-index">{index + 1}</span>
              <p>{step}</p>
            </div>
          ))}
        </Group>
        <Group title="关键规则">
          {rules.map((rule) => (
            <div className="step" key={rule}>
              <span className="rule-icon">
                <Icon name="check" size={15} />
              </span>
              <p>{rule}</p>
            </div>
          ))}
        </Group>
      </div>
    </section>
  )
}
