# Avaliação de QUIC vs Laminar para transferência de arquivos via UDP

## Resumo executivo
Laminar simplifica o uso de UDP mas não entrega controles de fluxo ou congestionamento maduros; isso explica os bugs graves observados. QUIC (via libs como [`quinn`]) oferece confiabilidade stream-based, congestion control moderno e suporte nativo a TLS/0-RTT, sendo melhor opção para transferência de arquivos. A migração exige ajustes arquiteturais (sessões, streams e handshake), porém elimina a necessidade de gambiarras de backpressure e reduz risco de perda de pacotes em redes reais.

## Limitações atuais com Laminar
- **Controle de fluxo frágil**: Laminar expõe apenas canais ordenados/unreliable. O backpressure é manual (`manual_poll` + retries). Isso não impede enfileiramento excessivo quando a aplicação produz dados mais rápido que a rede, causando os bugs reportados.
- **Sem congestion control**: não há algoritmo integrado; em redes com perda/jitter o throughput oscila e a latência explode.
- **Handshake mínimo**: não há segurança nem detecção de caminho; NAT traversal fica por conta da aplicação (perda de pacotes durante perfuração degrada ainda mais a fila de retransmissões).
- **Manutenção do projeto**: Laminar está pouco ativo; correções de bugs e tuning são limitados.

## Benefícios de QUIC (usando `quinn` em Rust)
- **Streams confiáveis e independentes**: cada arquivo pode usar um stream unidirecional. Retransmissões e ordering ficam a cargo do protocolo, evitando bloqueios head-of-line entre arquivos.
- **Controle de fluxo + congestion control embutidos**: QUIC implementa mecanismos similares ao TCP (CUBIC/NewReno) e flow control por stream/conexão, evitando saturar buffers.
- **Handshake seguro (TLS 1.3)**: autentica pares e habilita uso futuro de criptografia ponta a ponta sem alterar a lógica de aplicação.
- **Multiplexação e migração de caminho**: uma conexão QUIC pode sobreviver a mudanças de IP/porta (útil em NATs móveis) e dividir tráfego em streams paralelos.
- **Suporte ativo**: `quinn` é mantido, aderente às RFCs, e já usado em produção.

## Impactos de arquitetura na migração
- **Camada de sessão**: substituir `laminar::Socket` por `quinn::Endpoint` + `Connection`. A perfuração de NAT/STUN continua necessária para descobrir endereços, mas o transporte passa a ser QUIC.
- **Modelo de envio**: cada arquivo vira um stream unidirecional (`connection.open_uni`). Escrever o arquivo no stream remove a lógica manual de `FileChunk` e `FileDone` e o backpressure customizado.
- **Recepção**: ler de `incoming_uni` streams e mapear cada stream a um arquivo; metadados podem ir em um frame inicial ou stream separado.
- **Eventos/controle**: cancelamentos podem fechar o stream (`reset`) ou a conexão. O comando `CancelTransfers` não precisa enviar mensagens customizadas.
- **Configuração**: definir certificados autoassinados (ou `insecure` para P2P local), ajustar limites de janela (streams/data) e tunar `idle_timeout` para sessões de longa duração.

## Pontos de atenção e trade-offs
- **Tamanho binário**: `quinn` adiciona dependências de TLS; o binário cresce comparado ao Laminar.
- **Complexidade de handshake**: exige troca de certificados/keys. Em modo P2P simples, é possível usar certificados autoassinados e validar apenas fingerprints.
- **Compatibilidade**: QUIC depende de UDP sem interferência de middleboxes. A maioria dos NATs recentes suporta, mas firewalls antigos podem bloquear.
- **Migração gradual**: possível manter Laminar para descoberta/punch e usar QUIC apenas para a transferência de arquivos, reduzindo risco inicial.

## Recomendação
Migrar o transporte de arquivos para QUIC (`quinn`) para eliminar problemas de fluxo e congestionamento. O esforço inclui reestruturar o pipeline de transferência em streams unidirecionais e implementar handshake TLS simples, mas os ganhos em estabilidade e throughput justificam a troca.
