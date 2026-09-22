# Changelog

## 1.3.0 — 2026-09-22

### À savoir avant de mettre à jour

- **Licences Pro existantes** : au premier lancement de la 1.3.0, une connexion internet est nécessaire pour renouveler le jeton de licence (signé par le serveur). Hors ligne, l'app reste en plan gratuit jusqu'à la prochaine connexion.
- Les licences sont désormais liées à l'appareil et expirent selon la date décidée par le serveur.

### Sécurité

- Plus aucun secret dans l'application : les feedbacks passent par le serveur TransferBridge, avec limite de débit.
- Envoi de fichiers plus robuste : écriture directe sur disque (mémoire constante), session vérifiée avant toute lecture, nom de fichier assaini, aucun fichier existant écrasé.
- Code PIN : verrouillage par adresse IP et renouvellement automatique après 20 échecs cumulés.
- CORS retiré du serveur local et CSP activée dans l'application.
- Correction de failles XSS sur la page mobile (noms de fichiers).
- Le composant `cloudflared` (mode Relay cloud) est téléchargé dans une version épinglée et vérifié par SHA-256.

### Licences

- Jeton de licence signé (Ed25519) : modifier `license.json` ne donne plus accès aux fonctions Pro.
- Révocation prise en compte sous 24 h maximum (vérification quotidienne).
- « Déconnecter » libère réellement l'appareil pour réutiliser la clé ailleurs.
- Le plan gratuit n'est plus accordé par erreur à un autre appareil que celui de la licence.
- Le compteur d'envois quotidien survit au redémarrage.

### Interface

- Application et page mobile traduites en français, anglais et espagnol (langue détectée automatiquement).
- Le mode Relay cloud fonctionne avec la page mobile en HTTPS (WebSocket sécurisé).
- La page mobile est notifiée quand un fichier est envoyé depuis le PC.
- Les plans payants sont réellement illimités en taille de fichier.
- Historique limité à 7 jours en plan gratuit, comme annoncé.
- L'offre ne promet plus le chiffrement de bout en bout ; les fonctions à venir sont marquées « bientôt ».

### Technique

- Version affichée sur la page mobile lue automatiquement depuis l'application.
- Dépendances inutilisées retirées, README réécrit, tests unitaires ajoutés (sécurité, licences).
