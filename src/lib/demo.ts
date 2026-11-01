import {
  ApiError,
  TEMPLATES,
  type Action,
  type CaseDetail,
  type ReviewApi,
} from "./api"

type Seed = {
  id: number
  userId: string
  nickname: string
  content: string
  createdAt: string
  compositionId: string
  title: string
  description: string
  tags: string[]
  reports: [reason: string, content: string][]
  action: Action
  violation: string
  reason: string
}

const SEEDS: Seed[] = [
  {
    id: 53635,
    userId: "10248",
    nickname: "星河漫游者",
    content:
      "免费送存档码！！复制 8837 2910 4471 2209 进游戏就能满级，名额有限先到先得",
    createdAt: "2026-01-17 12:20",
    compositionId: "pki6666J",
    title: "像素冒险之旅",
    description: "一个关于探索、解谜与成长的原创平台跳跃游戏。",
    tags: ["平台跳跃", "原创"],
    reports: [
      ["其他", "26-1-14 违规散播存档码 无意义"],
      ["广告", "在多个作品下刷存档码"],
    ],
    action: 3,
    violation: "违规散播存档码",
    reason:
      "评论包含可直接使用的存档码，并带有诱导性表述，属于违规散播存档码。",
  },
  {
    id: 53634,
    userId: "20817",
    nickname: "一只小橘猫",
    content: "你做的什么垃圾作品，这也好意思发出来？",
    createdAt: "2026-01-17 11:52",
    compositionId: "qm72Lx9a",
    title: "星空钢琴",
    description: "用代码演奏的一段星空主题钢琴曲，支持键盘交互。",
    tags: ["音乐", "互动"],
    reports: [["人身攻击", "辱骂作者"]],
    action: 2,
    violation: "不友善言论",
    reason: "评论使用侮辱性词语贬低他人作品，未涉及其他违规，建议替换处理。",
  },
  {
    id: 53633,
    userId: "31175",
    nickname: "代码里的夏天",
    content: "这个关卡好有创意！请问最后一个机关是怎么实现的？",
    createdAt: "2026-01-17 11:30",
    compositionId: "ab31Kd0p",
    title: "机关迷城",
    description: "解开城中层层机关，找到通往钟楼的道路。",
    tags: ["解谜"],
    reports: [["其他", "疑似刷评论"]],
    action: 1,
    violation: "无违规",
    reason: "评论为正常的作品交流与提问，未发现违规。",
  },
  {
    id: 53632,
    userId: "40562",
    nickname: "晚风与星辰",
    content: "互赞互关，关注我的人我都会回关，大家一起冲榜！",
    createdAt: "2026-01-17 10:58",
    compositionId: "zz90Tq1c",
    title: "雨夜小镇",
    description: "一段雨夜漫步的互动短篇故事。",
    tags: ["故事", "氛围"],
    reports: [
      ["垃圾信息", "互关引流"],
      ["垃圾信息", "在多个作品下重复发布"],
      ["其他", "刷屏"],
    ],
    action: 2,
    violation: "互关引流",
    reason: "评论与作品无关，以互关、冲榜为目的批量发布，属于引流类垃圾信息。",
  },
  {
    id: 53631,
    userId: "52390",
    nickname: "Pixel_小北",
    content: "作者更新太慢了吧～～ 等到花儿都谢了",
    createdAt: "2026-01-17 10:41",
    compositionId: "pf55Rw2e",
    title: "方块农场",
    description: "种植、收获、建造属于自己的方块农场。",
    tags: ["模拟经营"],
    reports: [["不友善", "阴阳怪气"]],
    action: 1,
    violation: "无违规",
    reason: "评论语气调侃，但没有攻击性内容，属于正常的催更反馈。",
  },
]

const wait = (ms = 280) => new Promise((resolve) => setTimeout(resolve, ms))

function toDetail(seed: Seed): CaseDetail {
  return {
    reportId: seed.id,
    reportNum: seed.reports.length,
    status: 0,
    checkerId: 0,
    targetType: "comment",
    content: seed.content,
    commentCreatedAt: seed.createdAt,
    author: {
      userId: seed.userId,
      nickname: seed.nickname,
      avatar: "",
      autograph: "",
    },
    composition: {
      compositionId: seed.compositionId,
      title: seed.title,
      description: seed.description,
      tags: seed.tags,
      sourceUrl: "",
      coverUrl: "",
      publishTime: seed.createdAt,
      viewCount: 0,
      likeCount: 0,
      commentCount: 0,
      collectCount: 0,
      sbVersion: 3,
    },
    reports: seed.reports.map(([reasonName, content], index) => ({
      id: seed.id * 10 + index,
      content,
      reasonName,
      images: [],
      createdAt: "2026-01-17 12:36",
      reporter: {
        userId: String(9000 + index),
        nickname: `社区用户 ${1024 + index * 37}`,
        avatar: "",
        autograph: "",
      },
    })),
    judgeRecords: null,
    isDeal: false,
  }
}

/** In-memory API used for previewing the interface without the Rust service. */
export function createDemoApi(): ReviewApi {
  let cases = [...SEEDS]
  const find = (id: number) => {
    const seed = cases.find((item) => item.id === id)
    if (!seed) throw new ApiError("评论已经被删除", 422, 1070002)
    return seed
  }

  return {
    async session() {
      await wait()
      return {
        unread: { messageUnread: { comment: 2, system: 1 } },
        aiEnabled: true,
        community: { hasToken: true, masked: "demo-token" },
      }
    },
    async aiStatus() {
      await wait(120)
      return {
        enabled: true,
        model: "demo-model",
        keySource: "builtin" as const,
        keys: [
          {
            index: 1,
            masked: "sk-dem…0001",
            state: "healthy" as const,
            cooldownSecs: 0,
            remaining: 10,
            rateLimit: 10,
            failures: 0,
            lastError: "",
          },
        ],
      }
    },
    async cases(page, pageSize, options = {}) {
      await wait()
      const matching = options.reportId
        ? cases.filter((seed) => seed.id === options.reportId)
        : cases
      const start = (page - 1) * pageSize
      return {
        total: matching.length,
        page,
        pageSize,
        list: matching.slice(start, start + pageSize).map((seed) => ({
          id: seed.id,
          userId: seed.userId,
          nickname: seed.nickname,
          content: seed.content,
          checkerName: "",
          compositionId: seed.compositionId,
        })),
        normalTotal: matching.length,
        hiddenTotal: 0,
        hiddenClaimedTotal: 0,
        hiddenStaleTotal: 0,
        hiddenPage: 1,
        hiddenPageSize: pageSize,
        hiddenList: [],
        search: Boolean(options.reportId),
        truncated: false,
      }
    },
    async detail(id) {
      await wait()
      return toDetail(find(id))
    },
    async suggest(id) {
      await wait(900)
      const seed = find(id)
      return {
        action: seed.action,
        violationType: seed.violation,
        remark: seed.action === 1 ? "经核查未发现违规" : seed.violation,
        userMessage: TEMPLATES[seed.action].user,
        reporterMessage: TEMPLATES[seed.action].reporter,
        reason: seed.reason,
        confidence: 0.9,
      }
    },
    async verify(id) {
      await wait(500)
      find(id)
      cases = cases.filter((item) => item.id !== id)
    },
    async compositionCases(page, pageSize) {
      await wait()
      return {
        total: 0,
        page,
        pageSize,
        list: [],
        normalTotal: 0,
        hiddenTotal: 0,
        hiddenClaimedTotal: 0,
        hiddenStaleTotal: 0,
        hiddenPage: 1,
        hiddenPageSize: pageSize,
        hiddenList: [],
        search: false,
        truncated: false,
      }
    },
    async compositionDetail() {
      await wait()
      throw new Error("示例模式没有作品审核案件")
    },
    async verifyComposition() {
      await wait(300)
    },
    async judged() {
      await wait(150)
      return {
        list: [
          {
            id: 90001,
            judgeRecordId: 90001,
            reportType: 3,
            judgeTimeFormat: "2026-01-17 12:40",
            targetObject: { content: "免费送存档码，复制进游戏就能满级" },
            fromUserObject: { nickname: "星河漫游者" },
          },
        ],
        total: 1,
      }
    },
    async refreshSatisfaction(items) {
      await wait(150)
      return {
        items: items.map((item) => ({
          caseId: item.caseId,
          judgeRecordId: item.judgeRecordId || item.caseId * 2,
          satisfaction: 0.8,
          fetched: true,
        })),
      }
    },
    async serverAuto() {
      await wait(120)
      return {
        enabled: false,
        configured: false,
        tokenMasked: "****",
        interval: 60,
        threshold: 0.6,
        preferDelete: true,
        lastRunAt: null,
        lastMessage: "尚未配置审核 token",
        today: { submitted: 0, waiting: 0, failed: 0, stale: 0 },
      }
    },
    async setServerAuto(_patch) {
      await wait(120)
      return {
        enabled: false,
        configured: false,
        tokenMasked: "****",
        interval: 60,
        threshold: 0.6,
        preferDelete: true,
        lastRunAt: null,
        lastMessage: "尚未配置审核 token",
        today: { submitted: 0, waiting: 0, failed: 0, stale: 0 },
      }
    },
    async records() {
      await wait(160)
      return { records: [], tokenMasked: "demo-token" }
    },
    async syncedHistory() {
      await wait(120)
      return { records: [], scannedPages: 0, truncated: false, reviewerId: 0 }
    },
    async compositionSyncedHistory() {
      await wait(120)
      return { records: [], scannedPages: 0, truncated: false, reviewerId: 0 }
    },
    async deleteRecord() {
      await wait(120)
    },
    async setJudgeRecordId() {
      await wait(120)
    },
    async version() {
      return { build: "demo", version: "1.0.3" }
    },
    async prefs() {
      await wait(100)
      return { data: {}, updatedAt: 0 }
    },
    async savePrefs() {
      await wait(100)
    },
  }
}
