-- SPDX-FileCopyrightText: 2026 Andrew Hundt
-- SPDX-License-Identifier: Apache-2.0
insert into sessions(id,provider,provider_session_id,preview_text,source_path,parse_version,discovery_source)
values('claude:human-analytics','claude','human-analytics','','/synthetic/human-analytics','test','test');
insert into messages(id,session_id,provider,seq,role,kind,content,authorship,record_relation,input_origin) values
(901,'claude:human-analytics','claude',0,'user','conversation','you forgot direct','human','original','direct_conversation'),
(902,'claude:human-analytics','claude',1,'tool','tool_result','you forgot interactive','human','original','interactive_answer'),
(903,'claude:human-analytics','claude',2,'slash','conversation','you forgot slash','human','original','direct_conversation'),
(904,'claude:human-analytics','claude',3,'user','conversation','you forgot queued human','human','original','queued'),
(905,'claude:human-analytics','claude',4,'tool','conversation','you forgot queued unknown','unknown','original','queued'),
(906,'claude:human-analytics','claude',5,'user','conversation','you forgot mirror','human','mirror','direct_conversation'),
(907,'claude:human-analytics','claude',6,'tool','conversation','you forgot direct','human','original','queued'),
(908,'claude:human-analytics','claude',7,'tool','tool_result','you forgot generated','generated','original',null),
(909,'claude:human-analytics','claude',8,'user','harness_notice','you forgot notice','harness','original',null),
(910,'claude:human-analytics','claude',9,'tool','tool_result','generated | you forgot mixed','mixed','original','interactive_answer'),
(911,'claude:human-analytics','claude',10,'user','conversation','you forgot direct','human','original','direct_conversation');
insert into message_content_parts(message_id,ordinal,start_char,end_char,authorship,origin) values
(910,0,0,12,'generated','tool_payload'),(910,1,12,28,'human','direct_input');
