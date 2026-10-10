import { invoke } from '@tauri-apps/api/core'

// ─── 记忆模块 ───
// 所有记忆数据存在 %APPDATA%/desktop-pet/memory.db（SQLite），由 Rust 端管理
// 首次启动时自动从旧 memory.json 迁移数据
// 这个文件只提供前端调用的薄封装

export interface MemoryEntry {
  id: number
  category: string       // 'fact' | 'preference'
  content: string
  importance: number
  created_at: number
}

export interface ChatEntry {
  id: number
  role: string
  content: string
  time: number
}

export interface MemoryDetail {
  user_name: string
  memories: MemoryEntry[]
  recent_messages: ChatEntry[]
}

export interface MemoryStats {
  user_name: string
  facts_count: number
  preferences_count: number
  history_count: number
}

/** 记录一条聊天消息到记忆 */
export function addChat(role: string, content: string) {
  return invoke('add_chat', { role, content })
}

/** 获取记忆统计信息 */
export function getMemoryInfo() {
  return invoke<MemoryStats>('get_memory_info')
}

/** 查看详细的记忆内容（事实、偏好、最近聊天） */
export function getMemoryDetail() {
  return invoke<MemoryDetail>('get_memory_detail')
}

/** 删除指定 ID 的记忆条目 */
export function deleteMemory(id: number) {
  return invoke<string>('delete_memory', { id })
}

/** 清空所有记忆 */
export function clearMemories() {
  return invoke<string>('clear_memories')
}

/** 删除指定 ID 的聊天消息 */
export function deleteMessage(id: number) {
  return invoke<string>('delete_message', { id })
}

/** 清空所有聊天记录 */
export function clearMessages() {
  return invoke<string>('clear_messages')
}
