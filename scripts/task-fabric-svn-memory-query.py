#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = [
#     "fastembed>=0.5.0",
#     "numpy>=1.26.0",
#     "urllib3>=2.0.0",
# ]
# ///
"""Task Fabric SVN Exocortex Semantic Query & Local LLM Synthesis.

Performs vector cosine similarity search over ~/.local/state/task-fabric-svn/exocortex.db
and synthesizes answers via local llama-server (task-fabric-svn-semantic-qwen3-4b on 127.0.0.1:8080).
Supports concise voice-mode output for Kokoro TTS.
"""

import os
import sys
import json
import sqlite3
import argparse
import urllib.request
import urllib.error
from pathlib import Path
import numpy as np
from fastembed import TextEmbedding

STATE_DIR = Path(os.path.expanduser("~/.local/state/task-fabric-svn"))
DB_PATH = STATE_DIR / "exocortex.db"
MODEL_NAME = "BAAI/bge-small-en-v1.5"
LLAMA_SERVER_URL = os.getenv("TASK_FABRIC_SVN_LLAMA_URL", "http://127.0.0.1:8080/v1/chat/completions")
LLAMA_MODEL = "task-fabric-svn-semantic-qwen3-4b"

_embedder = None


def get_embedder():
    global _embedder
    if _embedder is None:
        _embedder = TextEmbedding(MODEL_NAME)
    return _embedder


def get_connection():
    if not DB_PATH.exists():
        raise RuntimeError(f"Exocortex database not found at {DB_PATH}. Run 'task-fabric-svn-exocortex-sync' first.")
    conn = sqlite3.connect(str(DB_PATH))
    conn.row_factory = sqlite3.Row
    return conn


def search_memories(query_text: str, world_id: str = None, limit: int = 5, min_score: float = 0.25):
    conn = get_connection()
    cursor = conn.cursor()

    if world_id:
        cursor.execute("""
            SELECT id, source, source_id, world_id, title, content_text, metadata_json, embedding, created_at
            FROM memories
            WHERE world_id = ? OR world_id IS NULL
        """, (world_id,))
    else:
        cursor.execute("""
            SELECT id, source, source_id, world_id, title, content_text, metadata_json, embedding, created_at
            FROM memories
        """)

    rows = cursor.fetchall()
    if not rows:
        conn.close()
        return []

    # Unpack embeddings
    embeddings = []
    metadata = []
    for r in rows:
        vec = np.frombuffer(r["embedding"], dtype=np.float32)
        embeddings.append(vec)
        metadata.append(dict(r))

    embeddings_matrix = np.array(embeddings, dtype=np.float32)

    # Generate query embedding
    embedder = get_embedder()
    query_vec = list(embedder.embed([query_text]))[0]
    query_vec = np.array(query_vec, dtype=np.float32)

    # Cosine similarity (vectors are L2-normalized)
    scores = np.dot(embeddings_matrix, query_vec)

    # Sort descending
    top_indices = np.argsort(scores)[::-1]

    results = []
    for idx in top_indices[:limit]:
        score = float(scores[idx])
        if score < min_score:
            continue
        item = metadata[idx]
        del item["embedding"]
        item["score"] = round(score, 4)
        results.append(item)

    conn.close()
    return results


def synthesize_with_llm(question: str, memories: list, voice_mode: bool = False) -> str:
    """Send retrieved context and question to local llama-server."""
    if not memories:
        return "I found no relevant records in workstation memory."

    context_lines = []
    for idx, m in enumerate(memories, 1):
        context_lines.append(
            f"[{idx}] Source: {m.get('source')}, World: {m.get('world_id') or 'system'}\n"
            f"Title: {m.get('title')}\n"
            f"Details: {m.get('content_text')}\n"
        )
    context_str = "\n".join(context_lines)

    if voice_mode:
        system_instruction = (
            "You are Task Fabric SVN, an ambient cognitive assistant. Answer the user's question directly and concisely "
            "using the retrieved workstation memory context below. "
            "Limit your answer to 1 or 2 plain spoken sentences. Do not use markdown, bullet points, or lists."
        )
    else:
        system_instruction = (
            "You are Task Fabric SVN's Longitudinal Cognitive Memory assistant. Answer the user's question based strictly on "
            "the provided workstation memory records. Be factual, concise, and highlight specific events, decisions, and dates when known."
        )

    user_prompt = f"Retrieved Workstation Memories:\n{context_str}\n\nQuestion: {question}\n\nAnswer:"

    payload = {
        "model": LLAMA_MODEL,
        "messages": [
            {"role": "system", "content": system_instruction},
            {"role": "user", "content": user_prompt},
        ],
        "temperature": 0.2,
        "max_tokens": 120 if voice_mode else 350,
    }

    req_data = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(
        LLAMA_SERVER_URL,
        data=req_data,
        headers={"Content-Type": "application/json"},
        method="POST",
    )

    try:
        with urllib.request.urlopen(req, timeout=25) as response:
            res_json = json.loads(response.read().decode("utf-8"))
            answer = res_json["choices"][0]["message"]["content"].strip()
            return answer
    except Exception as e:
        # Fallback to extractive summary if local LLM is unreachable or times out
        top = memories[0]
        return f"(Local LLM offline: {e}) Top memory: {top.get('title')}. Details: {top.get('content_text')[:180]}..."


def recall_world(world_id: str, limit: int = 3):
    conn = get_connection()
    cursor = conn.cursor()
    cursor.execute("""
        SELECT source, title, content_text, created_at
        FROM memories
        WHERE world_id = ? OR content_text LIKE ?
        ORDER BY created_at DESC
        LIMIT ?
    """, (world_id, f"%{world_id}%", limit))
    rows = cursor.fetchall()
    conn.close()
    return [dict(r) for r in rows]


def main():
    parser = argparse.ArgumentParser(description="Task Fabric SVN Exocortex Semantic Query Interface")
    subparsers = parser.add_subparsers(dest="command", help="Command to execute")

    # search subcommand
    p_search = subparsers.add_parser("search", help="Semantic vector search")
    p_search.add_argument("query", type=str, help="Search query string")
    p_search.add_argument("--world", type=str, default=None, help="Optional world ID filter")
    p_search.add_argument("--limit", type=int, default=5, help="Number of results to return")
    p_search.add_argument("--json", action="store_true", help="Output raw JSON")

    # query subcommand
    p_query = subparsers.add_parser("query", help="Ask question with local LLM synthesis")
    p_query.add_argument("question", type=str, help="Question to answer")
    p_query.add_argument("--world", type=str, default=None, help="Optional world ID filter")
    p_query.add_argument("--voice", action="store_true", help="Concise speech format for Kokoro TTS")
    p_query.add_argument("--limit", type=int, default=5, help="Number of context records")
    p_query.add_argument("--json", action="store_true", help="Output JSON with context and answer")

    # recall subcommand
    p_recall = subparsers.add_parser("recall", help="Recall recent memories for a world")
    p_recall.add_argument("--world", type=str, required=True, help="World ID to recall")
    p_recall.add_argument("--limit", type=int, default=3, help="Number of memories")
    p_recall.add_argument("--json", action="store_true", help="Output raw JSON")

    args = parser.parse_args()

    if not args.command:
        parser.print_help()
        sys.exit(1)

    if args.command == "search":
        results = search_memories(args.query, world_id=args.world, limit=args.limit)
        if args.json:
            print(json.dumps(results, indent=2))
        else:
            print(f"Exocortex Semantic Matches for: '{args.query}'\n" + "=" * 60)
            if not results:
                print("No relevant memories found.")
            for i, r in enumerate(results, 1):
                print(f"[{i}] Score: {r['score']} | {r['source']} | World: {r.get('world_id') or 'ambient'}")
                print(f"    Title: {r['title']}")
                print(f"    Snippet: {r['content_text'].splitlines()[0]}")
                print()

    elif args.command == "query":
        memories = search_memories(args.question, world_id=args.world, limit=args.limit)
        answer = synthesize_with_llm(args.question, memories, voice_mode=args.voice)
        if args.json:
            print(json.dumps({
                "question": args.question,
                "answer": answer,
                "context": memories,
            }, indent=2))
        else:
            if not args.voice:
                print(f"Question: {args.question}")
                print("-" * 60)
            print(answer)

    elif args.command == "recall":
        items = recall_world(args.world, limit=args.limit)
        if args.json:
            print(json.dumps(items, indent=2))
        else:
            print(f"Cognitive Recall for World '{args.world}':")
            for i, item in enumerate(items, 1):
                print(f"  {i}. [{item['source']}] {item['title']}")


if __name__ == "__main__":
    main()
