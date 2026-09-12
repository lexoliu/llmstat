-- Assistant inference calls in [lo, hi). chat_message is compact JSON with a
-- fixed key order: {"message_id":"<36-byte uuid>","role":"assistant",...
-- Both invariants are anchored with byte-prefix checks instead of json_extract
-- on the multi-KB blob (which would pull every overflow page). Verified over
-- the whole table: the filters match exactly the rows where
-- json_extract($.role) = 'assistant', and substr(16,36) equals $.message_id
-- for every one of them. If Devin ever changes the serialization, rows are
-- missed — never merged.
SELECT row_id,
       session_id,
       substr(chat_message, 16, 36) AS mid,
       COALESCE(json_extract(metadata, '$.num_tokens_preceding'), 0) AS ntp,
       created_at
FROM message_nodes
WHERE row_id >= ?1
  AND row_id < ?2
  AND substr(chat_message, 1, 15) = '{"message_id":"'
  AND substr(chat_message, 1, 120) LIKE '%"role":"assistant"%'
