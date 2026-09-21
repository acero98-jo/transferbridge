# TransferBridge

Application de bureau (Tauri 2 + React) pour transférer des fichiers entre un téléphone et un PC, sans câble ni compte : le PC affiche un QR code et un PIN, le téléphone ouvre la page et envoie ses fichiers.

## Fonctionnement

- Le PC lance un serveur HTTP local (axum, port 3030) qui sert la page mobile ([src-tauri/src/mobile_ui.html](src-tauri/src/mobile_ui.html)).
- Le téléphone scanne le QR code, saisit le PIN à 4 chiffres, puis envoie ses fichiers vers le dossier de réception du PC.
- **Plan gratuit** : téléphone vers PC, 10 envois par jour, 500 Mo par fichier, historique 7 jours.
- **Plans Pro** : PC vers téléphone, envois et taille illimités, mode Relay cloud (tunnel Cloudflare pour les transferts hors Wi-Fi).

## Développement

Prérequis : Node.js 20+, Rust stable, [prérequis Tauri](https://tauri.app/start/prerequisites/).

```bash
npm install
npm run tauri dev      # lance l'app en développement
cd src-tauri && cargo test --lib   # tests unitaires du backend
```

## Release

Pousser un tag `vX.Y.Z` déclenche [.github/workflows/release.yml](.github/workflows/release.yml), qui construit l'installeur Windows, le signe et publie la release ainsi que `latest.json` (utilisé par l'auto-update). Secrets requis : `TAURI_SIGNING_PRIVATE_KEY` et `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`.

## Services externes

L'app appelle un Worker Cloudflare (`WORKER_URL` dans [src-tauri/src/lib.rs](src-tauri/src/lib.rs)) pour :

- la vérification des licences (`POST /`) ;
- l'envoi des feedbacks (`POST /feedback`), qui relaie vers Discord. Le webhook Discord est un secret du Worker et ne doit jamais figurer dans ce dépôt. Voir [worker/feedback-relay.js](worker/feedback-relay.js).

## Sécurité

- PIN à 4 chiffres avec verrouillage par adresse IP et rotation automatique après 20 échecs cumulés.
- Sessions de 10 minutes, jeton requis avant toute lecture d'un fichier envoyé.
- Fichiers reçus écrits en flux sur disque, avec nom assaini (pas de chemin, pas de nom réservé Windows) et sans jamais écraser un fichier existant.
- Le binaire `cloudflared` est téléchargé dans une version épinglée et vérifié par SHA-256 avant exécution.
- Sur le réseau local, le trafic est en HTTP non chiffré : à n'utiliser que sur un Wi-Fi de confiance. Le mode Relay cloud passe en HTTPS.
