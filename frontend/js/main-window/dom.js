//! 集中 DOM 元素引用：单一查询源，避免各模块重复 getElementById。

export const appEl = document.getElementById("app");
export const queryEl = document.getElementById("query");
export const resultsEl = document.getElementById("results");
