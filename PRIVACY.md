# Privacy notice (DRAFT)

> Draft written from what the code does. It has not been reviewed by a lawyer, is not legal advice and gives no legal guarantee. Every duration marked `TO CONFIRM (legal)` is a proposal, not a decision.

- [English](#english)
- [Français](#français)

## English

### Who is responsible

squeezegram is a Windows app. Its Premium licence is sold by a Telegram bot run by the owner: Yanis Ethvignot (azellaxmc@gmail.com). The owner is the controller of the data described below for the bot and the licence signer. For the app itself, see "On your computer".

### On your computer

The app has no analytics or telemetry and no server of its own. It stores, only on your PC:

- settings, the queue and the licence key in `%APPDATA%\squeezegram`;
- the Telegram session (an encrypted TDLib database, its key protected by Windows DPAPI), ffmpeg, the journal and temporary files in `%LOCALAPPDATA%\squeezegram`.

The journal keeps 7 days at most (50 MB cap). It does not contain your phone number, codes, passwords, e-mail address or captions. It does contain the paths of the files you queue. It is only exported if you create a debug report yourself: the zip masks every path (`<path>/file-N`), hashes chat identifiers, and removes secrets, phone numbers and e-mail addresses. Check the zip before you share it.

The app contacts Telegram (what you choose to send), your proxy if you set one, and GitHub (ffmpeg download on first setup, and a check for a newer version at most once every 24 hours, which you can turn off). Nothing beyond a standard web request is sent to GitHub: no identifier, Windows version or usage data. Telegram receives a device name (by default "squeezegram" followed by your computer's name, which you can change in Settings) and the app version; they appear in Telegram under Settings > Devices. The licence is checked on your PC without going online.

The uninstaller keeps your data. To delete it, follow the "Privacy" section of the [README](README.md).

### What the bot and the signer keep, and why

When you buy Premium, Telegram gives the bot the payment details. The bot and the signer, which run on the owner's server (located in France), keep:

| Data                                                                                                                                                                                    | Where                         | Why                                                                                     |
| --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------- | --------------------------------------------------------------------------------------- |
| Your Telegram user id                                                                                                                                                                   | bot database                  | deliver your key again (`/mykey`), refund, answer support                               |
| Order: Telegram charge id, amount, currency, status, dates, internal invoice reference                                                                                                  | bot database                  | deliver the licence, refund, prevent a payment from being replayed or a double purchase |
| Licence key, and the id of a key revoked by a refund                                                                                                                                    | bot database                  | deliver it again, revoke it after a refund                                              |
| Date and language of your agreement to the terms of sale (including giving up the 14-day withdrawal)                                                                                    | bot database                  | prove the agreement                                                                     |
| Signature record: charge id, pseudonymous licence subject (`sub`), key id, licence id, licence, dates                                                                                   | signer database               | sign once per payment and return the same licence for a replay                          |
| Your support message, your user id, your language and your latest order                                                                                                                 | sent to the owner in Telegram | answer you after `/paysupport`                                                          |
| Charge ids of licences with no matching payment, found by the periodic payment audit (`python -m signer audit` reads the bot's Stars transactions from Telegram, `getStarTransactions`) | signer database               | check that each licence matches a payment (fraud prevention)                            |
| Hashed identifiers (not the raw id) in technical logs                                                                                                                                   | server logs                   | diagnose faults                                                                         |

The bot does not store your name, username or phone number. The licence contains a pseudonym derived from your Telegram account, not your id. Anyone holding the licence could check whether it belongs to a given account, so a licence is not anonymous.

### Who receives it

- **Telegram** is the payment platform (Telegram Stars) and the channel for the bot. It handles your payment and messages under its own terms and privacy policy.
- **GitHub** receives ordinary web requests when the app downloads ffmpeg or checks for updates.
- No one else. Nothing is sold or used for advertising.

### Legal basis (`TO CONFIRM (legal)`)

| Purpose                                                                      | Proposed legal basis | Status             |
| ---------------------------------------------------------------------------- | -------------------- | ------------------ |
| Deliver and re-send your key, handle refunds and support                     | Contract             | TO CONFIRM (legal) |
| Keep accounting records of orders                                            | Legal obligation     | TO CONFIRM (legal) |
| Prevent replayed payments, payment audit, technical logs, proof of agreement | Legitimate interest  | TO CONFIRM (legal) |

Telegram (payment and messaging platform) and GitHub (downloads) process data outside the EU under their own terms (`TO CONFIRM (legal)`).

### How long

| Data                                                          | Proposed duration                                        | Status             |
| ------------------------------------------------------------- | -------------------------------------------------------- | ------------------ |
| App data on your PC                                           | Until you delete it. Journal: 7 days                     | In the code        |
| Unused invoices and their consent entries (no payment made)   | 30 days, then deleted with `python -m bot purge`         | TO CONFIRM (legal) |
| Orders, charge ids, licence, consent date, refund information | 10 years after payment (accounting and dispute evidence) | TO CONFIRM (legal) |
| Signature records and revoked licence ids                     | Same as orders                                           | TO CONFIRM (legal) |
| Support messages in the owner's Telegram chat                 | 12 months after the request is closed                    | TO CONFIRM (legal) |
| Server logs                                                   | Kept in memory only, erased when the server restarts     | TO CONFIRM (legal) |
| Backups                                                       | Same duration as the data they contain                   | TO CONFIRM (legal) |

Orders and signatures are kept on purpose: they stop a successful payment from being replayed to obtain a second licence, and they let the owner handle refunds and revoke a refunded licence. This is why they are not deleted when you ask. Today the purge commands (`python -m bot purge`, `python -m signer purge`) delete only unused invoices and their consent entries. They do not delete orders or signatures, so deleting those after the period above needs a separate, reviewed tool or a manual action (`TO CONFIRM (legal)`). Deleting a row does not securely erase disk pages or backups.

### Your rights

Depending on the law that applies to you, you may ask for access, correction, deletion, restriction or portability of your data, or object to its use. Send the request with `/paysupport` in the bot, from the Telegram account that paid. The owner may need that account to find your data, and may refuse deletion of what must be kept (see above). Answer time: `[e.g. one month]` (`TO CONFIRM (legal)`). You can also complain to your data protection authority.

### Changes

This notice may change. The version in the repository is the current one.

## Français

### Qui est responsable

squeezegram est une application Windows. Sa licence Premium est vendue par un bot Telegram géré par le propriétaire : Yanis Ethvignot (azellaxmc@gmail.com). Le propriétaire est responsable du traitement des données décrites ci-dessous pour le bot et le signataire de licences. Pour l'application elle-même, voir « Sur votre ordinateur ».

### Sur votre ordinateur

L'application n'a ni analyse d'usage ni télémétrie, et aucun serveur propre. Elle stocke, uniquement sur votre PC :

- les réglages, la file d'attente et la clé de licence dans `%APPDATA%\squeezegram` ;
- la session Telegram (une base TDLib chiffrée, dont la clé est protégée par Windows DPAPI), ffmpeg, le journal et les fichiers temporaires dans `%LOCALAPPDATA%\squeezegram`.

Le journal est conservé 7 jours au plus (plafond de 50 Mo). Il ne contient ni votre numéro de téléphone, ni codes, ni mots de passe, ni adresse e-mail, ni légendes. Il contient en revanche les chemins des fichiers que vous mettez en file. Il n'est exporté que si vous créez vous-même un rapport de débogage : le zip masque chaque chemin (`<path>/file-N`), hache les identifiants de discussion et retire les secrets, numéros de téléphone et adresses e-mail. Vérifiez le zip avant de le partager.

L'application contacte Telegram (ce que vous choisissez d'envoyer), votre proxy si vous en avez défini un, et GitHub (téléchargement de ffmpeg à la première configuration, et recherche d'une nouvelle version au plus une fois toutes les 24 heures, que vous pouvez désactiver). Rien de plus qu'une requête web ordinaire n'est envoyé à GitHub : aucun identifiant, version de Windows ni donnée d'usage. Telegram reçoit un nom d'appareil (par défaut « squeezegram » suivi du nom de votre ordinateur, que vous pouvez changer dans les Réglages) et la version de l'application ; ils apparaissent dans Telegram sous Paramètres > Appareils. La licence est vérifiée sur votre PC, sans connexion.

Le désinstallateur conserve vos données. Pour les supprimer, suivez la section « Privacy » du [README](README.md).

### Ce que le bot et le signataire conservent, et pourquoi

Quand vous achetez Premium, Telegram transmet au bot les informations du paiement. Le bot et le signataire, qui tournent sur le serveur du propriétaire (situé en France), conservent :

| Donnée                                                                                                                                                                                                            | Où                                    | Pourquoi                                                                          |
| ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------- | --------------------------------------------------------------------------------- |
| Votre identifiant utilisateur Telegram                                                                                                                                                                            | base du bot                           | vous renvoyer votre clé (`/mykey`), rembourser, répondre au support               |
| Commande : identifiant de paiement Telegram, montant, devise, statut, dates, référence interne de facture                                                                                                         | base du bot                           | livrer la licence, rembourser, empêcher le rejeu d'un paiement ou un double achat |
| Clé de licence, et identifiant d'une clé révoquée par un remboursement                                                                                                                                            | base du bot                           | la renvoyer, la révoquer après un remboursement                                   |
| Date et langue de votre accord aux conditions de vente (dont la renonciation au droit de rétractation de 14 jours)                                                                                                | base du bot                           | prouver l'accord                                                                  |
| Enregistrement de signature : identifiant de paiement, sujet pseudonyme de la licence (`sub`), identifiant de clé, identifiant de licence, licence, dates                                                         | base du signataire                    | ne signer qu'une fois par paiement et renvoyer la même licence en cas de rejeu    |
| Votre message au support, votre identifiant, votre langue et votre dernière commande                                                                                                                              | envoyés au propriétaire dans Telegram | vous répondre après `/paysupport`                                                 |
| Identifiants de paiement des licences sans paiement correspondant, trouvés par l'audit périodique des paiements (`python -m signer audit` lit les transactions Stars du bot chez Telegram, `getStarTransactions`) | base du signataire                    | vérifier que chaque licence correspond à un paiement (prévention de la fraude)    |
| Identifiants hachés (jamais l'identifiant brut) dans les journaux techniques                                                                                                                                      | journaux du serveur                   | diagnostiquer les pannes                                                          |

Le bot ne stocke ni votre nom, ni votre nom d'utilisateur, ni votre numéro de téléphone. La licence contient un pseudonyme dérivé de votre compte Telegram, pas votre identifiant. Quiconque détient la licence peut vérifier si elle correspond à un compte donné : une licence n'est donc pas anonyme.

### Qui les reçoit

- **Telegram** est la plateforme de paiement (Telegram Stars) et le canal du bot. Il traite votre paiement et vos messages selon ses propres conditions et sa politique de confidentialité.
- **GitHub** reçoit des requêtes web ordinaires quand l'application télécharge ffmpeg ou cherche une mise à jour.
- Personne d'autre. Rien n'est vendu ni utilisé pour de la publicité.

### Base juridique (`À CONFIRMER (juridique)`)

| Finalité                                                                                     | Base juridique proposée | Statut                  |
| -------------------------------------------------------------------------------------------- | ----------------------- | ----------------------- |
| Livrer et renvoyer votre clé, traiter remboursements et support                              | Contrat                 | À CONFIRMER (juridique) |
| Tenir la comptabilité des commandes                                                          | Obligation légale       | À CONFIRMER (juridique) |
| Empêcher le rejeu de paiements, audit des paiements, journaux techniques, preuve de l'accord | Intérêt légitime        | À CONFIRMER (juridique) |

Telegram (plateforme de paiement et de messagerie) et GitHub (téléchargements) traitent des données hors de l'UE selon leurs propres conditions (`À CONFIRMER (juridique)`).

### Combien de temps

| Donnée                                                                                            | Durée proposée                                                     | Statut                  |
| ------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------ | ----------------------- |
| Données de l'application sur votre PC                                                             | Jusqu'à ce que vous les supprimiez. Journal : 7 jours              | Dans le code            |
| Factures non utilisées et leurs entrées de consentement (sans paiement)                           | 30 jours, puis suppression avec `python -m bot purge`              | À CONFIRMER (juridique) |
| Commandes, identifiants de paiement, licence, date de consentement, informations de remboursement | 10 ans après le paiement (comptabilité et preuve en cas de litige) | À CONFIRMER (juridique) |
| Enregistrements de signature et identifiants de licences révoquées                                | Comme les commandes                                                | À CONFIRMER (juridique) |
| Messages au support dans la discussion Telegram du propriétaire                                   | 12 mois après la clôture de la demande                             | À CONFIRMER (juridique) |
| Journaux du serveur                                                                               | Gardés en mémoire seulement, effacés au redémarrage du serveur     | À CONFIRMER (juridique) |
| Sauvegardes                                                                                       | Même durée que les données qu'elles contiennent                    | À CONFIRMER (juridique) |

Les commandes et les signatures sont conservées volontairement : elles empêchent qu'un paiement réussi soit rejoué pour obtenir une seconde licence, et permettent au propriétaire de traiter les remboursements et de révoquer une licence remboursée. C'est pourquoi elles ne sont pas supprimées sur simple demande. Aujourd'hui, les commandes de purge (`python -m bot purge`, `python -m signer purge`) ne suppriment que les factures non utilisées et leurs entrées de consentement. Elles ne suppriment ni les commandes ni les signatures : les supprimer après la durée ci-dessus demande un outil distinct, relu, ou une action manuelle (`À CONFIRMER (juridique)`). Supprimer une ligne n'efface pas de façon sûre les pages du disque ni les sauvegardes.

### Vos droits

Selon la loi qui vous est applicable, vous pouvez demander l'accès à vos données, leur rectification, leur effacement, la limitation de leur traitement ou leur portabilité, ou vous opposer à leur usage. Envoyez la demande avec `/paysupport` dans le bot, depuis le compte Telegram qui a payé. Le propriétaire peut avoir besoin de ce compte pour retrouver vos données, et peut refuser d'effacer ce qui doit être conservé (voir plus haut). Délai de réponse : `[par ex. un mois]` (`À CONFIRMER (juridique)`). Vous pouvez aussi saisir votre autorité de protection des données.

### Modifications

Cette notice peut changer. La version du dépôt est la version en vigueur.
