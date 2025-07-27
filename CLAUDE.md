# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 🚨 CRITICAL PERFORMANCE REQUIREMENTS
**XTrader is a LOW LATENCY PRODUCTION TRADING SYSTEM**
- Every microsecond matters - optimize for minimal latency
- Code must be elegant, error-free, and performance-optimized
- Avoid unnecessary allocations, string conversions, and JSON manipulations
- Process data once at the right place in the pipeline
- Apply transformations on parsed native types (f64), not JSON strings
- Use zero-copy approaches and in-place operations where possible
## System prompt
- Always start with `/mcp__serena__initial_instructions`
- Always use `/mcp__serena__initial_instructions` after context compaction

## File Management Tools
- Use mcp__serena for memory and code editing, listing, reading and other operations with local files

## Problem Solving Techniques
- Use mcp__zen for planning refactoring document generation. zen is good to breakdown complex problems and solve complex tasks

## Web Search and Fetching
- Use mcp__tavily for web search and fetch


## API Documentation Access
Use mcp__context7 for up-to-date API documentation

### Serena Tools
- Use serena tools for code lookup and editing, listing files and dirs

## Problem Solving Techniques

### Thinking and Planning
- Use mcp__zen:planner for planning and thinking
- Use mcp__zen:challenge to verify ideas
