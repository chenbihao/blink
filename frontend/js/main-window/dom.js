//! 集中 DOM 元素引用：单一查询源，避免各模块重复 getElementById。

export const appEl = document.getElementById("app");
export const queryEl = document.getElementById("query");
export const resultsEl = document.getElementById("results");

// 0.17.6: AI 模式元素
export const searchModeEl = document.getElementById("search-mode");
export const aiModeEl = document.getElementById("ai-mode");
export const aiQueryEl = document.getElementById("ai-query");
export const aiDisplayEl = document.getElementById("ai-display");
export const aiToolLineEl = document.getElementById("ai-tool-line");
export const aiRoundsEl = document.getElementById("ai-rounds");
export const aiContentEl = document.getElementById("ai-content");
