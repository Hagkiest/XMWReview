export type Action = 1 | 2 | 3

export type CaseSummary = {
  id: number
  userId: string
  nickname: string
  content: string
  checkerName: string
  compositionId: string
  createdAt?: string
  /** 审核员社区用户 ID（已审核列表用于匹配）。 */
  checkerId?: number
  /** 最后举报时间（Unix 秒）。 */
  lastReportTime?: number
  /** 作品字段（评论案件为空）。 */
  title?: string
  sourceUrl?: string
  coverUrl?: string
  reportNum?: number
}

export type CasePage = {
  /** 正常案件总数（搜索时为匹配到的正常案件数）。 */
  total: number
  page: number
  pageSize: number
  list: CaseSummary[]
  /** 正常案件总数，用于正常列表的分页。 */
  normalTotal?: number
  /** 隐藏案件（已认领 / 超过一周）总数，用于隐藏列表的分页。 */
  hiddenTotal?: number
  hiddenClaimedTotal?: number
  hiddenStaleTotal?: number
  hiddenPage?: number
  hiddenPageSize?: number
  hiddenList?: CaseSummary[]
  /** 本次是否为案件号搜索结果。 */
  search?: boolean
  /** 分类扫描达到页数上限，统计只覆盖前面部分。 */
  truncated?: boolean
}

export type CasePageOptions = {
  /** 按案件号搜索（上游 reportId 参数）。 */
  reportId?: number
  /** 隐藏案件自己的页码；不传为 1。 */
  hiddenPage?: number
  /** 内部调用（自动审核）直接转发上游分页，不做分类扫描。 */
  raw?: boolean
  /** 翻页/切模式时中断上一次整表扫描，避免请求在服务端排队堆积。 */
  signal?: AbortSignal
}

export type UserBrief = {
  userId: string
  nickname: string
  avatar: string
  autograph: string
}

export type ReportRecord = {
  id: number
  content: string
  reasonName: string
  images: string[]
  createdAt: string
  reporter: UserBrief
}

export type CaseDetail = {
  reportId: number
  reportNum: number
  status: number
  checkerId: number
  targetType: string
  content: string
  commentCreatedAt: string
  author: UserBrief
  composition: {
    compositionId: string
    title: string
    description: string
    tags: string[]
    /** 作品网页；为空时前端用 compositionId 拼社区地址。 */
    sourceUrl: string
    coverUrl: string
    publishTime: string
    viewCount: number
    likeCount: number
    commentCount: number
    collectCount: number
    sbVersion: number
  }
  reports: ReportRecord[]
  judgeRecords: unknown
  isDeal: boolean
}

export type Suggestion = {
  action: Action
  violationType: string
  remark: string
  userMessage: string
  reporterMessage: string
  reason: string
  confidence: number
}

export type VerifyInput = {
  action: Action
  remark: string
  userMessage: string
  reporterMessage: string
}

export type Session = {
  unread: { messageUnread?: Record<string, number>; total?: number; pending?: number; claimed?: number }
  aiEnabled: boolean
  /** 社区 access-token 是否有效（服务端用它反查审核员）。 */
  community?: CommunityState
}

export type CommunityState = {
  hasToken: boolean
  masked: string
}

/** 后端 /api/ai-status 返回的密钥轮转状态，密钥本身永远是掩码形式。 */
export type AiKeyStatus = {
  index: number
  masked: string
  state: "healthy" | "cooldown" | "throttled"
  cooldownSecs: number
  remaining: number
  rateLimit: number
  failures: number
  lastError: string
}

export type AiStatus = {
  enabled: boolean
  model?: string
  keySource?: "env" | "builtin"
  keys: AiKeyStatus[]
}

/** 后端 /api/judged 原样转发社区返回，结构按字段容错解析。 */
export type JudgedPage = unknown

/** 后端 /api/auto-config 返回的服务器 24 小时自动审核状态。 */
export type ServerAutoStatus = {
  enabled: boolean
  configured: boolean
  tokenMasked: string
  interval: number
  threshold: number
  preferDelete: boolean
  lastRunAt: number | null
  lastMessage: string
  today: { submitted: number; waiting: number; failed: number; stale: number }
}

export type ServerAutoPatch = {
  enabled?: boolean
  token?: string
  interval?: number
  threshold?: number
  preferDelete?: boolean
}

/** 审核记录：开源版只存在服务端 data 目录的 `records.json` 里，按 token 归属。 */
export type RecordEntry = {
  caseId: number
  judgeRecordId: number
  nickname: string
  content: string
  /** 0 = 来自已审核列表，具体处理结果未知。 */
  action: Action | 0
  remark: string
  /** auto = 服务器自动审核，manual = 人工提交，synced = 已审核列表同步 */
  source: "auto" | "manual" | "synced"
  confidence: number | null
  satisfaction: number | null
  satisfactionFetched: boolean
  /** 上次刷新满意度的时间（epoch 毫秒），0 = 还没刷过 */
  satisfactionAt: number
  /** 举报时间（不是评论发布时间） */
  caseReportTime: string
  /** epoch 毫秒 */
  createdAt: number
  tokenMasked: string
  /** 只在浏览器本地记过、服务端 records.json 里没有的那条 */
  pending?: boolean
}

export type RecordsResponse = {
  records: RecordEntry[]
  /** 当前 token 的掩码，界面用它说明「历史记录只对本 token 可见」。 */
  tokenMasked?: string
}

/** 已审核案件中属于当前审核员的一条（历史同步来源）。 */
export type SyncedHistoryEntry = {
  caseId: number
  nickname: string
  content: string
  /** Unix 秒；列表里只有这个时间字段 */
  lastReportTime: number
}

export type SyncedHistoryResponse = {
  records: SyncedHistoryEntry[]
  scannedPages: number
  truncated: boolean
  /** 当前 token 对应的社区审核员 ID */
  reviewerId: number
}

/** 已审核作品中属于当前审核员的一条。 */
export type SyncedCompositionEntry = {
  caseId: number
  nickname: string
  title: string
  lastReportTime: number
}

export type SyncedCompositionResponse = {
  records: SyncedCompositionEntry[]
  scannedPages: number
  truncated: boolean
  reviewerId: number
}

/** 刷新满意度时传的案件：判决记录号未知就传 0，服务端按案件号自己找。 */
export type SatisfactionItem = {
  caseId: number
  judgeRecordId: number
}

export type SatisfactionResult = {
  caseId: number
  judgeRecordId: number
  /** 0~1；null 表示还没有其他用户评价 */
  satisfaction: number | null
  /** 本次是否真的取到了（没取到下次再试） */
  fetched: boolean
}

export type ReviewApi = {
  session(): Promise<Session>
  aiStatus(): Promise<AiStatus>
  cases(page: number, pageSize: number, options?: CasePageOptions): Promise<CasePage>
  detail(id: number): Promise<CaseDetail>
  suggest(id: number): Promise<Suggestion>
  verify(id: number, input: VerifyInput): Promise<void>
  /** 作品审核：待审列表 / 详情 / 提交（不抓满意度）。 */
  compositionCases(
    page: number,
    pageSize: number,
    options?: CasePageOptions,
  ): Promise<CasePage>
  compositionDetail(id: number): Promise<CaseDetail>
  verifyComposition(id: number, input: VerifyInput): Promise<void>
  judged(page: number, pageSize: number): Promise<JudgedPage>
  /** 刷新满意度：服务端按案件号找判决记录号，再取赞同/反对数自己算比例。 */
  refreshSatisfaction(items: SatisfactionItem[]): Promise<{
    items: SatisfactionResult[]
  }>
  /** 服务器端 24 小时自动审核：查询状态 / 更新配置。 */
  serverAuto(): Promise<ServerAutoStatus>
  setServerAuto(patch: ServerAutoPatch): Promise<ServerAutoStatus>
  /** 当前 token 的审核记录（服务端 data 目录里的本地 JSON）。 */
  records(limit?: number): Promise<RecordsResponse>
  /** 已审核案件列表中属于当前审核员的历史（新的历史来源）。 */
  syncedHistory(options?: {
    limit?: number
    pages?: number
    /** 设置里手动填写的审核员 ID；填了优先使用 */
    reviewerId?: number
  }): Promise<SyncedHistoryResponse>
  /** 已审核作品列表中属于当前审核员的历史。 */
  compositionSyncedHistory(options: {
    reviewerId: number
    limit?: number
    pages?: number
  }): Promise<SyncedCompositionResponse>
  /** 删除一条历史审核记录。 */
  deleteRecord(caseId: number): Promise<void>
  /** 提交后补判决记录 ID。 */
  setJudgeRecordId(caseId: number, judgeRecordId: number): Promise<void>
  /** 前端页面版本指纹 + 版本号（用于检测重新打包后自动刷新）。 */
  version(): Promise<{ build: string; version: string }>
  /** 本机的偏好设置（按 access-token 归属，存在服务端 data 目录）。 */
  prefs(): Promise<{ data: Record<string, unknown>; updatedAt: number }>
  savePrefs(data: Record<string, unknown>): Promise<void>
}

export const ACTIONS: { value: Action; label: string }[] = [
  { value: 1, label: "正常显示" },
  { value: 2, label: "替换" },
  { value: 3, label: "删除" },
]

// Action 2/3 copy comes from the official review dialog; action 1 is a neutral template.
export const TEMPLATES: Record<Action, { reporter: string; user: string }> = {
  1: {
    reporter: "感谢您对社区内容的正义维护，经核查该内容未发现违规。",
    user: "经核查，您的评论未发现违规，已恢复正常显示。",
  },
  2: {
    reporter: "感谢您对社区内容的正义维护，您的举报已受理。",
    user: "您的评论存在违规内容，已被系统管理员删除。",
  },
  3: {
    reporter: "感谢您对社区内容的正义维护，您的举报已受理。",
    user: "您的评论存在违规内容，该评论及子回复已被系统管理员删除。",
  },
}

export class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly code?: number,
    /** 后端表示社区凭证不可用，前端据此回到粘贴 token 的入口。 */
    readonly authExpired: boolean = false,
  ) {
    super(message)
  }

  /** 只有在 token 缺失/失效时才为 true，业务错误不会把用户踢回入口页。 */
  get expired() {
    return this.authExpired
  }
}

const OFFLINE_MESSAGE = "无法连接 Rust 服务，请确认后端已启动"
const TIMEOUT_MESSAGE = "请求超时，请检查网络或 AI 服务"

// 旧实现没有超时，一个卡住的请求会让自动审核整个停摆，所以每个端点都设上限。
const TIMEOUTS = {
  // 分类分页要扫完整份列表，首次还要用详情核对举报时间，留足时间。
  cases: 60_000,
  detail: 20_000,
  suggest: 70_000,
  verify: 25_000,
  aiStatus: 8_000,
  // 满意度要扫判决列表 + 逐个取意见，比普通接口慢一些。
  satisfaction: 45_000,
  // 历史同步要按页扫描已审核案件列表，最多几十页。
  historySync: 90_000,
} as const

/** 请求身份：开源版只认社区 access-token，没有会话票据。 */
export type AuthOptions = {
  token?: string
}

/** 幂等 GET 的兜底重试：只在「连不上 / 超时」（status 0）时再试，业务错误立刻抛出。 */
async function getWithRetry<T>(
  fn: () => Promise<T>,
  attempts = 2,
  delayMs = 400,
): Promise<T> {
  for (let attempt = 1; ; attempt += 1) {
    try {
      return await fn()
    } catch (error) {
      if (
        attempt >= attempts ||
        !(error instanceof ApiError) ||
        error.status !== 0
      ) {
        throw error
      }
      await new Promise((resolve) => setTimeout(resolve, delayMs))
    }
  }
}

export function createApi(auth: AuthOptions = {}): ReviewApi {
  const token = (auth.token ?? "").trim()

  async function request<T>(
    path: string,
    init: RequestInit = {},
    timeoutMs: number = TIMEOUTS.detail,
    signal?: AbortSignal,
  ): Promise<T> {
    let response: Response
    // 超时和调用方中断共用一个 controller：谁先触发都要真的把 fetch 掐断，
    // 否则翻页/切模式后旧请求还在服务端跑整表扫描。
    const controller = new AbortController()
    const timer = setTimeout(() => controller.abort(), timeoutMs)
    const onAbort = () => controller.abort()
    signal?.addEventListener("abort", onAbort)
    try {
      response = await fetch(`/api${path}`, {
        ...init,
        signal: controller.signal,
        headers: {
          ...(token ? { "access-token": token } : {}),
          ...(init.body ? { "Content-Type": "application/json" } : {}),
        },
      })
    } catch (error) {
      if (signal?.aborted) throw error
      if (controller.signal.aborted) throw new ApiError(TIMEOUT_MESSAGE, 0)
      throw new ApiError(OFFLINE_MESSAGE, 0)
    } finally {
      clearTimeout(timer)
      signal?.removeEventListener("abort", onAbort)
    }
    const payload = await response.json().catch(() => null)
    if (!response.ok) {
      throw new ApiError(
        payload?.message ?? `请求失败（${response.status}）`,
        response.status,
        payload?.code,
        Boolean(payload?.authExpired),
      )
    }
    // A dev server without the Rust proxy answers with HTML instead of JSON.
    if (payload === null) throw new ApiError(OFFLINE_MESSAGE, 0)
    return payload as T
  }

  return {
    // 启动时的第一个请求：偶发的连不上/超时重试一次，避免整屏卡在加载动画。
    session: () =>
      getWithRetry(() => request<Session>("/session", {}, TIMEOUTS.cases)),
    aiStatus: () =>
      getWithRetry(() => request<AiStatus>("/ai-status", {}, TIMEOUTS.aiStatus)),
    cases: (page, pageSize, options = {}) => {
      const params = new URLSearchParams({
        page: String(page),
        pageSize: String(pageSize),
      })
      if (options.reportId) params.set("reportId", String(options.reportId))
      if (options.hiddenPage && options.hiddenPage > 1)
        params.set("hiddenPage", String(options.hiddenPage))
      if (options.raw) params.set("raw", "true")
      return request<CasePage>(
        `/cases?${params}`,
        {},
        TIMEOUTS.cases,
        options.signal,
      )
    },
    detail: (id) => request<CaseDetail>(`/cases/${id}`, {}, TIMEOUTS.detail),
    suggest: (id) =>
      request<Suggestion>(
        `/cases/${id}/ai-suggestion`,
        { method: "POST" },
        TIMEOUTS.suggest,
      ),
    verify: async (id, input) => {
      await request(
        `/cases/${id}/verify`,
        {
          method: "POST",
          body: JSON.stringify(input),
        },
        TIMEOUTS.verify,
      )
    },
    compositionCases: (page, pageSize, options = {}) => {
      const params = new URLSearchParams({
        page: String(page),
        pageSize: String(pageSize),
      })
      if (options.reportId) params.set("reportId", String(options.reportId))
      if (options.hiddenPage && options.hiddenPage > 1)
        params.set("hiddenPage", String(options.hiddenPage))
      if (options.raw) params.set("raw", "true")
      return request<CasePage>(
        `/composition/cases?${params}`,
        {},
        TIMEOUTS.cases,
        options.signal,
      )
    },
    compositionDetail: (id) =>
      request<CaseDetail>(
        `/composition/cases/${id}`,
        {},
        TIMEOUTS.detail,
      ),
    verifyComposition: async (id, input) => {
      await request(
        `/composition/cases/${id}/verify`,
        {
          method: "POST",
          body: JSON.stringify(input),
        },
        TIMEOUTS.verify,
      )
    },
    judged: (page, pageSize) =>
      request<JudgedPage>(
        "/judged",
        { method: "POST", body: JSON.stringify({ page, pageSize }) },
        TIMEOUTS.cases,
      ),
    refreshSatisfaction: (items) =>
      request<{ items: SatisfactionResult[] }>(
        "/records/satisfaction/refresh",
        { method: "POST", body: JSON.stringify({ items }) },
        TIMEOUTS.satisfaction,
      ),
    serverAuto: () =>
      request<ServerAutoStatus>("/auto-config", {}, TIMEOUTS.aiStatus),
    setServerAuto: (patch) =>
      request<ServerAutoStatus>(
        "/auto-config",
        { method: "POST", body: JSON.stringify(patch) },
        TIMEOUTS.aiStatus,
      ),
    records: (limit = 200) =>
      request<RecordsResponse>(
        `/records?limit=${limit}`,
        {},
        TIMEOUTS.cases,
      ),
    syncedHistory: (options = {}) => {
      const params = new URLSearchParams()
      if (options.limit) params.set("limit", String(options.limit))
      if (options.pages) params.set("pages", String(options.pages))
      if (options.reviewerId) params.set("reviewerId", String(options.reviewerId))
      const query = params.toString()
      return request<SyncedHistoryResponse>(
        `/history/synced${query ? `?${query}` : ""}`,
        {},
        TIMEOUTS.historySync,
      )
    },
    compositionSyncedHistory: (options) => {
      const params = new URLSearchParams({
        reviewerId: String(options.reviewerId),
      })
      if (options.limit) params.set("limit", String(options.limit))
      if (options.pages) params.set("pages", String(options.pages))
      return request<SyncedCompositionResponse>(
        `/composition/history/synced?${params}`,
        {},
        TIMEOUTS.historySync,
      )
    },
    deleteRecord: (caseId) =>
      request(
        `/records/${caseId}`,
        { method: "DELETE" },
        TIMEOUTS.cases,
      ).then(() => undefined),
    setJudgeRecordId: (caseId, judgeRecordId) =>
      request(
        "/records/judge-id",
        { method: "POST", body: JSON.stringify({ caseId, judgeRecordId }) },
        TIMEOUTS.cases,
      ).then(() => undefined),
    version: () =>
      request<{ build: string; version: string }>(
        "/version",
        {},
        TIMEOUTS.aiStatus,
      ),
    prefs: () =>
      request<{ data: Record<string, unknown>; updatedAt: number }>(
        "/prefs",
        {},
        TIMEOUTS.aiStatus,
      ),
    savePrefs: (data) =>
      request(
        "/prefs",
        { method: "POST", body: JSON.stringify({ data }) },
        TIMEOUTS.aiStatus,
      ).then(() => undefined),
  }
}
